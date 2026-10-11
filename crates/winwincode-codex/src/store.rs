// SPDX-License-Identifier: Apache-2.0

use std::collections::HashSet;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};

use crate::model_port_client::{
    ModelCancellationFingerprint, ModelCancellationPhase, ModelChunkFingerprint, ModelCursorHead,
    ModelCursorSnapshot, ModelCursorStore, ModelLeaseAuthority, ModelTerminationReason,
    valid_cancellation,
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
    ReplayAcknowledgementStore, ReplayFrame, ReplayReadQuery, ReplaySnapshot, ReplayStore,
    ReplayStreamKey,
};
use winwincode_execution_port::{
    generated::{ApprovalActionSanitizedDetail, ExecutionPortMessage, ModelChunkMessage},
    runtime_trace_outbox::{ExecutionMode, ObserverMode, PerformanceBaselineReport},
    typed_replay::frame_from_message,
};

#[cfg(test)]
#[path = "model_storage_incremental_tests.rs"]
mod model_storage_incremental_tests;

#[cfg(test)]
#[path = "input_interaction_store_tests.rs"]
mod input_interaction_store_tests;

pub(crate) const DATABASE_FILE: &str = "worker-codex.sqlite3";
const MAX_MODEL_CALL_FRAME_BYTES: usize = 16 * 1024 * 1024;
const MAX_SAFE_METRIC: i64 = 9_007_199_254_740_991;

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

fn extend_existing_model_lineage(
    transaction: &Transaction<'_>,
    run_key: &str,
    existing_root: Option<&(String, Vec<u8>)>,
    authority_json: &[u8],
) -> Result<bool, AdapterStoreError> {
    let Some((existing_run_key, bytes)) = existing_root else {
        return Ok(false);
    };
    let previous: ModelLeaseAuthority =
        serde_json::from_slice(bytes).map_err(|_| AdapterStoreError::Corrupt)?;
    let current: ModelLeaseAuthority =
        serde_json::from_slice(authority_json).map_err(|_| AdapterStoreError::Corrupt)?;
    if existing_run_key != run_key
        || !crate::model_bridge::same_binding_with_extended_lease(&previous, &current)
    {
        return Ok(false);
    }
    let mut statement = transaction
        .prepare("SELECT thread_id, authority_json FROM model_thread_lineage WHERE run_key = ?1")
        .map_err(|_| AdapterStoreError::Unavailable)?;
    let rows = statement
        .query_map([run_key], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
        })
        .map_err(|_| AdapterStoreError::Unavailable)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| AdapterStoreError::Unavailable)?;
    drop(statement);
    for (child_id, bytes) in rows {
        let mut child: ModelLeaseAuthority =
            serde_json::from_slice(&bytes).map_err(|_| AdapterStoreError::Corrupt)?;
        if !crate::model_bridge::common_authority(&previous, &child) {
            return Err(AdapterStoreError::Conflict);
        }
        child.lease.expires_at = current.lease.expires_at.clone();
        let bytes = serde_json::to_vec(&child).map_err(|_| AdapterStoreError::Corrupt)?;
        transaction.execute(
                            "UPDATE model_thread_lineage SET authority_json = ?1 WHERE thread_id = ?2 AND run_key = ?3",
                            params![bytes, child_id, run_key],
                        ).map_err(|_| AdapterStoreError::Unavailable)?;
    }
    Ok(true)
}

#[allow(clippy::too_many_lines)]
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
                 CREATE TABLE IF NOT EXISTS post_action_trace (
                   run_key TEXT NOT NULL, source_key TEXT NOT NULL, sequence INTEGER NOT NULL,
                   record_json BLOB NOT NULL, PRIMARY KEY(run_key,source_key), UNIQUE(run_key,sequence)
                 );
                 CREATE TABLE IF NOT EXISTS fusion_panel (
                   run_key TEXT NOT NULL,
                   panel_id TEXT NOT NULL,
                   input_json BLOB NOT NULL,
                   result_json BLOB,
                   PRIMARY KEY (run_key, panel_id)
                 );
                 CREATE TABLE IF NOT EXISTS codex_store_migration (
                   migration_key TEXT PRIMARY KEY NOT NULL
                 );
                 CREATE TABLE IF NOT EXISTS model_cursor (
                   stream_key TEXT PRIMARY KEY NOT NULL,
                   snapshot_json BLOB NOT NULL
                 );
                 CREATE TABLE IF NOT EXISTS model_cursor_frame (
                   stream_key TEXT NOT NULL REFERENCES model_cursor(stream_key),
                   sequence INTEGER NOT NULL CHECK(sequence > 0),
                   fingerprint_json BLOB NOT NULL,
                   PRIMARY KEY(stream_key, sequence)
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
                   jev_accounting_complete INTEGER NOT NULL DEFAULT 0
                     CHECK(jev_accounting_complete IN (0, 1)),
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
                 CREATE TABLE IF NOT EXISTS model_call_task_context (
                   run_key TEXT NOT NULL,
                   model_call_id TEXT NOT NULL,
                   request_digest TEXT NOT NULL,
                   context_digest TEXT NOT NULL,
                   context_text BLOB NOT NULL,
                   PRIMARY KEY (run_key, model_call_id)
                 );
                 CREATE TABLE IF NOT EXISTS model_call_response_seal (
                   run_key TEXT NOT NULL, model_call_id TEXT NOT NULL,
                   frame_count INTEGER NOT NULL CHECK(frame_count>0),
                   response_digest TEXT NOT NULL,
                   PRIMARY KEY(run_key,model_call_id),
                   FOREIGN KEY(run_key,model_call_id) REFERENCES model_call_ledger(run_key,model_call_id)
                 );
                 CREATE TRIGGER IF NOT EXISTS model_call_frame_sealed_insert BEFORE INSERT ON model_call_frame
                   WHEN EXISTS(SELECT 1 FROM model_call_response_seal WHERE run_key=NEW.run_key AND model_call_id=NEW.model_call_id)
                   BEGIN SELECT RAISE(ABORT,'sealed model response is immutable'); END;
                 CREATE TRIGGER IF NOT EXISTS model_call_frame_sealed_update BEFORE UPDATE ON model_call_frame
                   WHEN EXISTS(SELECT 1 FROM model_call_response_seal WHERE run_key=OLD.run_key AND model_call_id=OLD.model_call_id)
                     OR EXISTS(SELECT 1 FROM model_call_response_seal WHERE run_key=NEW.run_key AND model_call_id=NEW.model_call_id)
                   BEGIN SELECT RAISE(ABORT,'sealed model response is immutable'); END;
                 CREATE TRIGGER IF NOT EXISTS model_call_frame_sealed_delete BEFORE DELETE ON model_call_frame
                   WHEN EXISTS(SELECT 1 FROM model_call_response_seal WHERE run_key=OLD.run_key AND model_call_id=OLD.model_call_id)
                   BEGIN SELECT RAISE(ABORT,'sealed model response is immutable'); END;
                 CREATE TRIGGER IF NOT EXISTS model_call_response_seal_immutable BEFORE UPDATE ON model_call_response_seal
                   BEGIN SELECT RAISE(ABORT,'sealed model response metadata is immutable'); END;
                 CREATE TRIGGER IF NOT EXISTS model_call_response_seal_delete BEFORE DELETE ON model_call_response_seal
                   BEGIN SELECT RAISE(ABORT,'sealed model response metadata is immutable'); END;
                 CREATE TRIGGER IF NOT EXISTS model_call_response_seal_replace BEFORE INSERT ON model_call_response_seal
                   WHEN EXISTS(SELECT 1 FROM model_call_response_seal WHERE run_key=NEW.run_key AND model_call_id=NEW.model_call_id)
                   BEGIN SELECT RAISE(ABORT,'sealed model response metadata is immutable'); END;
                 CREATE TRIGGER IF NOT EXISTS model_call_sealed_binding_update
                   BEFORE UPDATE OF run_key,model_call_id,model_exchange_id,request_digest ON model_call_ledger
                   WHEN EXISTS(SELECT 1 FROM model_call_response_seal WHERE run_key=OLD.run_key AND model_call_id=OLD.model_call_id)
                     AND (NEW.run_key IS NOT OLD.run_key OR NEW.model_call_id IS NOT OLD.model_call_id
                       OR NEW.model_exchange_id IS NOT OLD.model_exchange_id OR NEW.request_digest IS NOT OLD.request_digest)
                   BEGIN SELECT RAISE(ABORT,'sealed model response binding is immutable'); END;
                 CREATE TRIGGER IF NOT EXISTS model_call_sealed_binding_delete BEFORE DELETE ON model_call_ledger
                   WHEN EXISTS(SELECT 1 FROM model_call_response_seal WHERE run_key=OLD.run_key AND model_call_id=OLD.model_call_id)
                   BEGIN SELECT RAISE(ABORT,'sealed model response binding is immutable'); END;
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
                 CREATE TABLE IF NOT EXISTS runtime_replay_legacy_compaction (
                     stream_key TEXT PRIMARY KEY NOT NULL,
                     compacted_through INTEGER NOT NULL
                 );
                 CREATE TABLE IF NOT EXISTS runtime_replay_head (
                   stream_key TEXT PRIMARY KEY NOT NULL,
                   ack_sequence INTEGER NOT NULL CHECK(ack_sequence >= 0),
                   highest_sequence INTEGER NOT NULL CHECK(highest_sequence >= ack_sequence)
                 );
                 CREATE TABLE IF NOT EXISTS runtime_replay_frame (
                   stream_key TEXT NOT NULL, sequence INTEGER NOT NULL CHECK(sequence > 0),
                   event_id TEXT NOT NULL, digest TEXT NOT NULL, frame BLOB NOT NULL,
                   PRIMARY KEY(stream_key, sequence), UNIQUE(stream_key, event_id)
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
                 CREATE INDEX IF NOT EXISTS execution_outbox_deliverable_position
                   ON execution_outbox(position) WHERE state = 'pending' OR acknowledgement_required = 1;
                 CREATE INDEX IF NOT EXISTS execution_outbox_model_exchange
                   ON execution_outbox(json_extract(frame_json,'$.modelExchangeId')) WHERE family='model_open';
                 CREATE TABLE IF NOT EXISTS execution_terminal_outcome (
                   run_key TEXT PRIMARY KEY NOT NULL,
                   frame_digest TEXT NOT NULL,
                   frame_json BLOB NOT NULL
                 );
                 INSERT OR IGNORE INTO execution_terminal_outcome(run_key, frame_digest, frame_json)
                   SELECT r.run_key, e.frame_digest, e.frame_json
                   FROM codex_run r JOIN execution_outbox e
                     ON e.delivery_id = json_extract(r.record_json, '$.terminalMessageId')
                   WHERE e.family = 'outcome' AND NOT EXISTS (
                     SELECT 1 FROM execution_terminal_outcome t WHERE t.run_key = r.run_key
                   );
                 CREATE TABLE IF NOT EXISTS execution_delivery_disposition (
                   delivery_id TEXT PRIMARY KEY NOT NULL,
                   original_digest TEXT NOT NULL,
                   original_frame BLOB NOT NULL,
                   projected_frame BLOB,
                   rejected INTEGER NOT NULL DEFAULT 0 CHECK(rejected IN (0,1))
                 );
                 CREATE TABLE IF NOT EXISTS execution_response_receipt (
                   family TEXT NOT NULL,
                   correlation_key TEXT NOT NULL,
                   response_digest TEXT NOT NULL,
                   PRIMARY KEY(family, correlation_key)
                 );
                 CREATE TABLE IF NOT EXISTS worker_transport_state (
                   state_key TEXT PRIMARY KEY NOT NULL,
                   sequence INTEGER NOT NULL CHECK(sequence >= 0)
                 );
                 CREATE TABLE IF NOT EXISTS approval_operation (
                   approval_id TEXT PRIMARY KEY NOT NULL,
                   run_key TEXT NOT NULL,
                   kernel_session_id TEXT NOT NULL,
                   operation_kind TEXT NOT NULL CHECK(operation_kind IN ('exec', 'patch', 'mcp')),
                   operation_id TEXT NOT NULL,
                   turn_id TEXT,
                   request_digest TEXT NOT NULL,
                   detail_json BLOB NOT NULL,
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
                   choice_identities_json BLOB NOT NULL,
                   resolution_digest TEXT,
                   state TEXT NOT NULL CHECK(state IN ('pending', 'resolved')),
                   CHECK (
                     (state = 'pending' AND resolution_digest IS NULL)
                     OR (state = 'resolved' AND resolution_digest IS NOT NULL)
                   )
                 );
                 CREATE TABLE IF NOT EXISTS input_call (
                   run_key TEXT NOT NULL,
                   request_digest TEXT NOT NULL,
                   record_json BLOB NOT NULL,
                   PRIMARY KEY(run_key, request_digest)
                 );",
            )
            .map_err(|_| AdapterStoreError::Unavailable)?;
    migrate_model_cursor_schema(connection)?;
    migrate_mcp_approval_schema(connection)?;
    initialize_performance_schema(connection)
}

fn migrate_mcp_approval_schema(connection: &Connection) -> Result<(), AdapterStoreError> {
    let transaction = connection
        .unchecked_transaction()
        .map_err(|_| AdapterStoreError::Unavailable)?;
    let schema: String = transaction
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'approval_operation'",
            [],
            |row| row.get(0),
        )
        .map_err(|_| AdapterStoreError::Unavailable)?;
    if !schema.contains("'mcp'") {
        let replacement = schema
            .replacen("approval_operation", "approval_operation_migrating", 1)
            .replace("'exec', 'patch'", "'exec', 'patch', 'mcp'");
        if !replacement.contains("'mcp'") {
            return Err(AdapterStoreError::Corrupt);
        }
        transaction
            .execute_batch(&replacement)
            .map_err(|_| AdapterStoreError::Unavailable)?;
        transaction
            .execute_batch(
                "INSERT INTO approval_operation_migrating SELECT * FROM approval_operation;
             DROP TABLE approval_operation;
             ALTER TABLE approval_operation_migrating RENAME TO approval_operation;",
            )
            .map_err(|_| AdapterStoreError::Unavailable)?;
    }
    transaction
        .commit()
        .map_err(|_| AdapterStoreError::Unavailable)
}

#[allow(clippy::too_many_lines)]
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
                 CREATE TABLE IF NOT EXISTS performance_jev_parent_terminal (
                   run_key TEXT NOT NULL,exchange_id TEXT NOT NULL,
                   model_call_id TEXT NOT NULL,observed_at TEXT NOT NULL,
                   PRIMARY KEY(run_key,exchange_id)
                 );
                 CREATE TABLE IF NOT EXISTS performance_jev_receipt (
                   run_key TEXT NOT NULL,
                   operation_id TEXT NOT NULL,
                   receipt_json TEXT NOT NULL,
                   completed INTEGER NOT NULL CHECK(completed IN (0,1)),
                   input_tokens INTEGER NOT NULL CHECK(input_tokens>=0),
                   output_tokens INTEGER NOT NULL CHECK(output_tokens>=0),
                   PRIMARY KEY(run_key,operation_id)
                 );
                 CREATE TABLE IF NOT EXISTS observer_settlement (
                   run_key TEXT NOT NULL,
                   batch_id TEXT NOT NULL,
                   settlement_json TEXT NOT NULL,
                   PRIMARY KEY(run_key,batch_id)
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
    for table in ["performance_operation", "performance_jev_receipt"] {
        let exists: bool = connection.query_row(&format!("SELECT EXISTS(SELECT 1 FROM pragma_table_info('{table}') WHERE name='usage_known')"), [], |row| row.get(0)).map_err(|_| AdapterStoreError::Unavailable)?;
        if !exists {
            connection.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN usage_known INTEGER NOT NULL DEFAULT 1 CHECK(usage_known IN (0,1));")).map_err(|_| AdapterStoreError::Unavailable)?;
        }
    }
    connection.execute_batch("CREATE TABLE IF NOT EXISTS performance_usage_settlement (
        run_key TEXT NOT NULL, operation_kind TEXT NOT NULL, operation_id TEXT NOT NULL,
        input_tokens INTEGER NOT NULL, cached_tokens INTEGER NOT NULL, output_tokens INTEGER NOT NULL,
        actual_cost_microunits INTEGER, observed_at TEXT NOT NULL,
        usage_known INTEGER NOT NULL CHECK(usage_known IN (0,1)), revision INTEGER NOT NULL,
        PRIMARY KEY(run_key,operation_kind,operation_id,revision));").map_err(|_|AdapterStoreError::Unavailable)?;
    for table in ["performance_operation", "performance_usage_settlement"] {
        let exists:bool=connection.query_row(&format!("SELECT EXISTS(SELECT 1 FROM pragma_table_info('{table}') WHERE name='cache_breakdown_known')"),[],|row|row.get(0)).map_err(|_|AdapterStoreError::Unavailable)?;
        if !exists {
            connection.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN cache_breakdown_known INTEGER NOT NULL DEFAULT 1 CHECK(cache_breakdown_known IN (0,1));")).map_err(|_|AdapterStoreError::Unavailable)?;
        }
    }
    connection.execute_batch("DROP VIEW IF EXISTS performance_operation_accounted;
        CREATE VIEW performance_operation_accounted AS
        SELECT p.run_key,p.operation_kind,p.operation_id,p.started_at,p.completed_at,p.completed,p.duration_millis,
          COALESCE(s.input_tokens,p.input_tokens) AS input_tokens,
          COALESCE(s.cached_tokens,p.cached_tokens) AS cached_tokens,
          COALESCE(s.output_tokens,p.output_tokens) AS output_tokens,
          CASE WHEN s.operation_id IS NOT NULL THEN s.actual_cost_microunits ELSE p.actual_cost_microunits END AS actual_cost_microunits,
          COALESCE(s.usage_known,p.usage_known) AS usage_known,
          COALESCE(s.cache_breakdown_known,p.cache_breakdown_known) AS cache_breakdown_known
        FROM performance_operation p LEFT JOIN performance_usage_settlement s
          ON s.run_key=p.run_key AND s.operation_kind=p.operation_kind AND s.operation_id=p.operation_id
          AND s.revision=(SELECT MAX(q.revision) FROM performance_usage_settlement q
            WHERE q.run_key=p.run_key AND q.operation_kind=p.operation_kind AND q.operation_id=p.operation_id);").map_err(|_| AdapterStoreError::Unavailable)?;
    let _ = connection.execute_batch("UPDATE performance_jev_receipt SET completed=1,
          usage_known=CASE WHEN json_extract(receipt_json,'$.run.observation.outputTokens') IS NOT NULL
            AND json_array_length(receipt_json,'$.run.failures')=0 THEN 1 ELSE 0 END
          WHERE json_type(receipt_json,'$.run')='object';
        UPDATE performance_jev_receipt SET usage_known=0 WHERE completed=0;
        UPDATE performance_operation SET completed=1, completed_at=COALESCE((SELECT json_extract(CAST(f.frame_json AS TEXT),'$.sentAt')
            FROM model_call_frame f WHERE f.run_key=performance_operation.run_key AND f.model_call_id=performance_operation.operation_id
            ORDER BY f.sequence DESC LIMIT 1),started_at), usage_known=0
        WHERE operation_kind='primary_model' AND completed=0 AND EXISTS (
          SELECT 1 FROM model_call_ledger m WHERE m.run_key=performance_operation.run_key
          AND m.model_call_id=performance_operation.operation_id AND m.completed=1);");
    Ok(())
}
fn validate_post_action_value(
    value: &serde_json::Value,
) -> Result<(&str, i64, bool), AdapterStoreError> {
    let _: crate::adapter::StoredPostActionTrace =
        serde_json::from_value(value.clone()).map_err(|_| AdapterStoreError::Corrupt)?;
    let source = value["sourceKey"]
        .as_str()
        .filter(|source| {
            !source.is_empty() && source.len() <= 253 && !source.chars().any(char::is_control)
        })
        .ok_or(AdapterStoreError::Corrupt)?;
    let sequence = value["sequence"]
        .as_i64()
        .filter(|sequence| *sequence > 0)
        .ok_or(AdapterStoreError::Corrupt)?;
    let retained = value["retained"]
        .as_bool()
        .ok_or(AdapterStoreError::Corrupt)?;
    let actions = value["actions"]
        .as_array()
        .filter(|actions| !actions.is_empty())
        .ok_or(AdapterStoreError::Corrupt)?;
    let mut unique = HashSet::new();
    if actions.iter().any(|action| !unique.insert(action.as_str())) {
        return Err(AdapterStoreError::Corrupt);
    }
    Ok((source, sequence, retained))
}

fn save_post_action_value(
    tx: &Transaction<'_>,
    run_key: &str,
    value: &serde_json::Value,
) -> Result<(), AdapterStoreError> {
    let (source, sequence, retained) = validate_post_action_value(value)?;
    let previous: Option<Vec<u8>> = tx
        .query_row(
            "SELECT record_json FROM post_action_trace WHERE run_key=?1 AND source_key=?2",
            params![run_key, source],
            |row| row.get(0),
        )
        .optional()
        .map_err(|_| AdapterStoreError::Unavailable)?;
    if let Some(previous) = previous {
        let mut existing: serde_json::Value =
            serde_json::from_slice(&previous).map_err(|_| AdapterStoreError::Corrupt)?;
        validate_post_action_value(&existing)?;
        if existing["retained"] == true && !retained {
            return Err(AdapterStoreError::Conflict);
        }
        existing["retained"] = retained.into();
        if existing != *value {
            return Err(AdapterStoreError::Conflict);
        }
    }
    let bytes = serde_json::to_vec(value).map_err(|_| AdapterStoreError::Corrupt)?;
    #[cfg(test)]
    crate::storage_mechanism_regression::post_action_write(bytes.len());
    tx.execute("INSERT INTO post_action_trace VALUES (?1,?2,?3,?4) ON CONFLICT(run_key,source_key) DO UPDATE SET record_json=excluded.record_json",params![run_key,source,sequence,bytes]).map_err(|_|AdapterStoreError::Conflict)?;
    Ok(())
}

fn migrate_runtime_outbox_index(connection: &Connection) -> Result<(), AdapterStoreError> {
    let tx = connection
        .unchecked_transaction()
        .map_err(|_| AdapterStoreError::Unavailable)?;
    for (column, kind) in [
        ("runtime_stream_key", "TEXT"),
        ("runtime_sequence", "INTEGER"),
    ] {
        let exists = tx
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM pragma_table_info('execution_outbox') WHERE name=?1)",
                [column],
                |row| row.get::<_, bool>(0),
            )
            .map_err(|_| AdapterStoreError::Unavailable)?;
        if !exists {
            tx.execute_batch(&format!(
                "ALTER TABLE execution_outbox ADD COLUMN {column} {kind};"
            ))
            .map_err(|_| AdapterStoreError::Unavailable)?;
        }
    }
    let migrated=tx.query_row("SELECT EXISTS(SELECT 1 FROM codex_store_migration WHERE migration_key='runtime-outbox-index-v1')",[],|row|row.get::<_,bool>(0)).map_err(|_|AdapterStoreError::Unavailable)?;
    if !migrated {
        let rows = {
            let mut stmt = tx
                .prepare(
                    "SELECT delivery_id,frame_json FROM execution_outbox WHERE family='runtime'",
                )
                .map_err(|_| AdapterStoreError::Unavailable)?;
            stmt.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
            })
            .map_err(|_| AdapterStoreError::Unavailable)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| AdapterStoreError::Unavailable)?
        };
        for (delivery, bytes) in rows {
            let message: ExecutionPortMessage =
                serde_json::from_slice(&bytes).map_err(|_| AdapterStoreError::Corrupt)?;
            let ExecutionPortMessage::RuntimeEventMessage(event) = message else {
                return Err(AdapterStoreError::Corrupt);
            };
            if event.event.sequence.0 <= 0 {
                return Err(AdapterStoreError::Corrupt);
            }
            tx.execute("UPDATE execution_outbox SET runtime_stream_key=?2,runtime_sequence=?3 WHERE delivery_id=?1",params![delivery,winwincode_execution_port::runtime_replay::runtime_event_stream_key(&event).as_str(),event.event.sequence.0]).map_err(|_|AdapterStoreError::Unavailable)?;
        }
        tx.execute(
            "INSERT INTO codex_store_migration VALUES ('runtime-outbox-index-v1')",
            [],
        )
        .map_err(|_| AdapterStoreError::Unavailable)?;
    }
    tx.execute_batch("CREATE INDEX IF NOT EXISTS execution_outbox_runtime_stream ON execution_outbox(family,runtime_stream_key,runtime_sequence,position);").map_err(|_|AdapterStoreError::Unavailable)?;
    tx.commit().map_err(|_| AdapterStoreError::Unavailable)
}

fn migrate_post_action_records(connection: &Connection) -> Result<(), AdapterStoreError> {
    let tx = connection
        .unchecked_transaction()
        .map_err(|_| AdapterStoreError::Unavailable)?;
    let migrated = tx
        .query_row(
            "SELECT 1 FROM codex_store_migration WHERE migration_key='post-action-indexed-v1'",
            [],
            |_| Ok(()),
        )
        .optional()
        .map_err(|_| AdapterStoreError::Unavailable)?
        .is_some();
    tx.execute_batch("CREATE TRIGGER IF NOT EXISTS post_action_block_legacy_insert BEFORE INSERT ON codex_run WHEN json_type(NEW.record_json,'$.postActionTraces') IS NOT NULL BEGIN SELECT RAISE(ABORT,'indexed post-action traces require current binary'); END;
        CREATE TRIGGER IF NOT EXISTS post_action_block_legacy_update BEFORE UPDATE OF record_json ON codex_run WHEN json_type(NEW.record_json,'$.postActionTraces') IS NOT NULL BEGIN SELECT RAISE(ABORT,'indexed post-action traces require current binary'); END;").map_err(|_| AdapterStoreError::Unavailable)?;
    if migrated {
        return tx.commit().map_err(|_| AdapterStoreError::Unavailable);
    }
    let rows = {
        let mut stmt=tx.prepare("SELECT run_key,record_json FROM codex_run WHERE json_type(record_json,'$.postActionTraces') IS NOT NULL").map_err(|_|AdapterStoreError::Unavailable)?;
        stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
        })
        .map_err(|_| AdapterStoreError::Unavailable)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| AdapterStoreError::Unavailable)?
    };
    for (run, bytes) in rows {
        let mut value: serde_json::Value =
            serde_json::from_slice(&bytes).map_err(|_| AdapterStoreError::Corrupt)?;
        let traces = value["postActionTraces"]
            .as_array()
            .ok_or(AdapterStoreError::Corrupt)?;
        let mut sources = HashSet::new();
        let mut sequences = HashSet::new();
        for trace in traces {
            let (source, sequence, _) = validate_post_action_value(trace)?;
            if !sources.insert(source) || !sequences.insert(sequence) {
                return Err(AdapterStoreError::Corrupt);
            }
            save_post_action_value(&tx, &run, trace)?;
        }
        value
            .as_object_mut()
            .ok_or(AdapterStoreError::Corrupt)?
            .remove("postActionTraces");
        tx.execute(
            "UPDATE codex_run SET record_json=?2 WHERE run_key=?1",
            params![
                run,
                serde_json::to_vec(&value).map_err(|_| AdapterStoreError::Corrupt)?
            ],
        )
        .map_err(|_| AdapterStoreError::Unavailable)?;
    }
    tx.execute(
        "INSERT INTO codex_store_migration VALUES ('post-action-indexed-v1')",
        [],
    )
    .map_err(|_| AdapterStoreError::Unavailable)?;
    tx.commit().map_err(|_| AdapterStoreError::Unavailable)
}

impl AdapterStore {
    pub(crate) fn open(root: &Path) -> Result<Self, AdapterStoreError> {
        std::fs::create_dir_all(root).map_err(|_| AdapterStoreError::Unavailable)?;
        restrict_directory(root)?;
        let path = root.join(DATABASE_FILE);
        create_private_file(&path)?;
        let connection = Connection::open(&path).map_err(|_| AdapterStoreError::Unavailable)?;
        initialize_schema(&connection)?;
        connection.execute_batch("CREATE TRIGGER IF NOT EXISTS runtime_replay_block_legacy_insert BEFORE INSERT ON runtime_replay BEGIN SELECT RAISE(ABORT, 'indexed runtime replay requires current binary'); END;
            CREATE TRIGGER IF NOT EXISTS runtime_replay_block_legacy_update BEFORE UPDATE ON runtime_replay BEGIN SELECT RAISE(ABORT, 'indexed runtime replay requires current binary'); END;").map_err(|_|AdapterStoreError::Unavailable)?;
        migrate_post_action_records(&connection)?;
        migrate_runtime_outbox_index(&connection)?;
        ensure_model_call_phase_columns(&connection)?;
        ensure_input_operation_choice_identities_column(&connection)?;
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

    fn load_run_header_in_connection(
        connection: &Connection,
        run_key: &str,
    ) -> Result<Option<serde_json::Value>, AdapterStoreError> {
        let bytes = connection
            .query_row(
                "SELECT record_json FROM codex_run WHERE run_key = ?1",
                params![run_key],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .optional()
            .map_err(|_| AdapterStoreError::Unavailable)?;
        let Some(bytes) = bytes else {
            return Ok(None);
        };
        #[cfg(test)]
        crate::storage_mechanism_regression::run_header_read(bytes.len());
        let value: serde_json::Value =
            serde_json::from_slice(&bytes).map_err(|_| AdapterStoreError::Corrupt)?;
        if value.get("postActionTraces").is_some() {
            return Err(AdapterStoreError::Corrupt);
        }
        Ok(Some(value))
    }

    pub(crate) fn load_run_header<T: DeserializeOwned>(
        &self,
        run_key: &str,
    ) -> Result<Option<T>, AdapterStoreError> {
        let connection = self.lock()?;
        let Some(value) = Self::load_run_header_in_connection(&connection, run_key)? else {
            return Ok(None);
        };
        serde_json::from_value(value)
            .map(Some)
            .map_err(|_| AdapterStoreError::Corrupt)
    }

    pub(crate) fn load_run<T: DeserializeOwned>(
        &self,
        run_key: &str,
    ) -> Result<Option<T>, AdapterStoreError> {
        let connection = self.lock()?;
        let Some(mut value) = Self::load_run_header_in_connection(&connection, run_key)? else {
            return Ok(None);
        };
        let mut statement = connection
            .prepare("SELECT source_key,sequence,record_json FROM post_action_trace WHERE run_key=?1 ORDER BY sequence")
            .map_err(|_| AdapterStoreError::Unavailable)?;
        let rows = statement
            .query_map([run_key], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                ))
            })
            .map_err(|_| AdapterStoreError::Unavailable)?;
        let mut traces = Vec::new();
        for row in rows {
            let (source, sequence, bytes) = row.map_err(|_| AdapterStoreError::Unavailable)?;
            #[cfg(test)]
            crate::storage_mechanism_regression::post_action_read(bytes.len());
            let trace: serde_json::Value =
                serde_json::from_slice(&bytes).map_err(|_| AdapterStoreError::Corrupt)?;
            let (record_source, record_sequence, _) = validate_post_action_value(&trace)?;
            #[cfg(test)]
            crate::storage_mechanism_regression::post_action_validation();
            if record_source != source || record_sequence != sequence {
                return Err(AdapterStoreError::Corrupt);
            }
            traces.push(trace);
        }
        if !traces.is_empty() {
            value["postActionTraces"] = serde_json::Value::Array(traces);
        }
        serde_json::from_value(value)
            .map(Some)
            .map_err(|_| AdapterStoreError::Corrupt)
    }

    pub(crate) fn save_post_action_trace<T: Serialize>(
        &self,
        run_key: &str,
        trace: &T,
    ) -> Result<(), AdapterStoreError> {
        let value = serde_json::to_value(trace).map_err(|_| AdapterStoreError::Corrupt)?;
        self.transaction(|tx| save_post_action_value(tx, run_key, &value))
    }

    pub(crate) fn save_run<T: Serialize>(
        &self,
        run_key: &str,
        record: &T,
    ) -> Result<(), AdapterStoreError> {
        let bytes = serde_json::to_vec(record).map_err(|_| AdapterStoreError::Corrupt)?;
        #[cfg(test)]
        crate::storage_mechanism_regression::run_write(bytes.len());
        self.lock()?
            .execute(
                "INSERT INTO codex_run(run_key, record_json) VALUES (?1, ?2)
                 ON CONFLICT(run_key) DO UPDATE SET record_json = excluded.record_json",
                params![run_key, bytes],
            )
            .map_err(|_| AdapterStoreError::Unavailable)?;
        Ok(())
    }

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

    pub(crate) fn record_performance_completion(
        &self,
        run_key: &str,
        kind: PerformanceOperationKind,
        operation_id: &str,
        completed_at: &winwincode_domain::Instant,
        completion: PerformanceOperationCompletion,
    ) -> Result<(), AdapterStoreError> {
        self.record_performance_completion_impl(
            run_key,
            kind,
            operation_id,
            completed_at,
            completion,
            false,
        )
    }

    pub(crate) fn close_unmeasured_primary(
        &self,
        run_key: &str,
        operation_id: &str,
        completed_at: &winwincode_domain::Instant,
    ) -> Result<(), AdapterStoreError> {
        self.record_performance_completion_impl(
            run_key,
            PerformanceOperationKind::PrimaryModel,
            operation_id,
            completed_at,
            PerformanceOperationCompletion {
                usage_known: false,
                ..Default::default()
            },
            true,
        )
    }

    #[allow(clippy::too_many_lines)]
    fn record_performance_completion_impl(
        &self,
        run_key: &str,
        kind: PerformanceOperationKind,
        operation_id: &str,
        completed_at: &winwincode_domain::Instant,
        completion: PerformanceOperationCompletion,
        only_if_pending: bool,
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
            if only_if_pending {
                return Ok(());
            }
            let original: (String,i64,i64,i64,i64,Option<i64>,bool,bool) = transaction.query_row(
                "SELECT completed_at,duration_millis,input_tokens,cached_tokens,output_tokens,actual_cost_microunits,usage_known,cache_breakdown_known FROM performance_operation WHERE run_key=?1 AND operation_kind=?2 AND operation_id=?3",
                params![run_key,kind.as_str(),operation_id], |r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?,r.get(7)?)))
                .map_err(|_| AdapterStoreError::Unavailable)?;
            let duration = completion
                .duration_millis
                .or_else(|| {
                    elapsed_millis(
                        &winwincode_domain::Instant(
                            existing.as_ref().expect("existing completion").0.clone(),
                        ),
                        completed_at,
                    )
                })
                .unwrap_or(0);
            let exact_original = original
                == (
                    completed_at.0.clone(),
                    duration,
                    completion.input_tokens,
                    completion.cached_tokens,
                    completion.output_tokens,
                    completion.actual_cost_microunits,
                    completion.usage_known,
                    completion.cache_breakdown_known,
                );
            transaction
                .commit()
                .map_err(|_| AdapterStoreError::Unavailable)?;
            drop(connection);
            if exact_original {
                return Ok(());
            }
            return self.settle_performance_usage(
                run_key,
                kind,
                operation_id,
                completed_at,
                completion,
            );
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
                   , actual_cost_microunits, usage_known, cache_breakdown_known
                 ) VALUES (?1, ?2, ?3, ?4, ?5, 1, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
                 ON CONFLICT(run_key, operation_kind, operation_id) DO UPDATE SET
                   completed_at = excluded.completed_at,
                   completed = 1,
                   duration_millis = excluded.duration_millis,
                   input_tokens = excluded.input_tokens,
                   cached_tokens = excluded.cached_tokens,
                   output_tokens = excluded.output_tokens,
                   actual_cost_microunits = excluded.actual_cost_microunits,
                   usage_known = excluded.usage_known,
                   cache_breakdown_known = excluded.cache_breakdown_known
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
                    completion.usage_known,
                    completion.cache_breakdown_known,
                ],
            )
            .map_err(|_| AdapterStoreError::Unavailable)?;
        transaction
            .commit()
            .map_err(|_| AdapterStoreError::Unavailable)
    }

    pub(crate) fn settle_performance_usage(
        &self,
        run_key: &str,
        kind: PerformanceOperationKind,
        operation_id: &str,
        observed_at: &winwincode_domain::Instant,
        usage: PerformanceOperationCompletion,
    ) -> Result<(), AdapterStoreError> {
        validate_performance_identity(run_key, operation_id, observed_at)?;
        for value in [usage.input_tokens, usage.cached_tokens, usage.output_tokens] {
            validate_metric(value)?;
        }
        if let Some(cost) = usage.actual_cost_microunits {
            validate_metric(cost)?;
        }
        let mut connection = self.lock()?;
        let tx = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|_| AdapterStoreError::Unavailable)?;
        let replay: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM performance_usage_settlement WHERE run_key=?1 AND operation_kind=?2 AND operation_id=?3 AND observed_at=?4 AND input_tokens=?5 AND cached_tokens=?6 AND output_tokens=?7 AND actual_cost_microunits IS ?8 AND usage_known=?9 AND cache_breakdown_known=?10)",
            params![run_key,kind.as_str(),operation_id,observed_at.0,usage.input_tokens,usage.cached_tokens,usage.output_tokens,usage.actual_cost_microunits,usage.usage_known,usage.cache_breakdown_known], |r|r.get(0)).map_err(|_| AdapterStoreError::Unavailable)?;
        if replay {
            return Ok(());
        }
        let row:(bool,bool,i64,i64,i64,Option<i64>,bool)=tx.query_row(
            "SELECT completed,usage_known,input_tokens,cached_tokens,output_tokens,actual_cost_microunits,cache_breakdown_known FROM performance_operation_accounted WHERE run_key=?1 AND operation_kind=?2 AND operation_id=?3",
            params![run_key,kind.as_str(),operation_id], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?))).map_err(|_| AdapterStoreError::Conflict)?;
        let values = (
            usage.input_tokens,
            usage.cached_tokens,
            usage.output_tokens,
            usage.actual_cost_microunits,
        );
        if !row.0
            || (row.1
                && (!usage.usage_known
                    || values.0.checked_add(values.1) != row.2.checked_add(row.3)
                    || (row.6
                        && (!usage.cache_breakdown_known
                            || values.0 != row.2
                            || values.1 != row.3))
                    || values.2 != row.4))
            || (!row.1 && values.0.checked_add(values.1) < row.2.checked_add(row.3))
            || values.2 < row.4
            || row.5.is_some_and(|cost| values.3 != Some(cost))
        {
            return Err(AdapterStoreError::Conflict);
        }
        if (usage.usage_known, usage.cache_breakdown_known, values)
            == (row.1, row.6, (row.2, row.3, row.4, row.5))
        {
            return Ok(());
        }
        let revision:i64=tx.query_row("SELECT COALESCE(MAX(revision),0)+1 FROM performance_usage_settlement WHERE run_key=?1 AND operation_kind=?2 AND operation_id=?3",params![run_key,kind.as_str(),operation_id], |r|r.get(0)).map_err(|_| AdapterStoreError::Unavailable)?;
        tx.execute(
            "INSERT INTO performance_usage_settlement VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
            params![
                run_key,
                kind.as_str(),
                operation_id,
                values.0,
                values.1,
                values.2,
                values.3,
                observed_at.0,
                usage.usage_known,
                revision,
                usage.cache_breakdown_known
            ],
        )
        .map_err(|_| AdapterStoreError::Unavailable)?;
        tx.commit().map_err(|_| AdapterStoreError::Unavailable)
    }

    pub(crate) fn execution_outcome_usage(
        &self,
        run_key: &str,
        runtime_millis: i64,
    ) -> Result<winwincode_execution_port::generated::ExecutionOutcomeUsage, AdapterStoreError>
    {
        let totals = self.delegated_performance_totals(run_key)?;
        Ok(
            winwincode_execution_port::generated::ExecutionOutcomeUsage {
                runtime_millis,
                tokens: totals.usage_complete.then_some(totals.total_tokens),
                known_tokens: totals.total_tokens,
                accounting_status: if totals.usage_complete {
                    winwincode_execution_port::generated::ExecutionOutcomeUsageAccountingStatus::Known
                } else {
                    winwincode_execution_port::generated::ExecutionOutcomeUsageAccountingStatus::Unknown
                },
                cost_microunits: totals.cost_complete.then_some(totals.total_cost_microunits),
            },
        )
    }

    pub(crate) fn retained_outcome_usage(
        &self,
        run_key: &str,
        runtime_millis: i64,
    ) -> Result<
        Option<winwincode_execution_port::generated::ExecutionOutcomeUsage>,
        AdapterStoreError,
    > {
        let totals = self.delegated_performance_totals(run_key)?;
        if totals.primary_model_calls + totals.observer_calls == 0 {
            return Ok(None);
        }
        self.execution_outcome_usage(run_key, runtime_millis)
            .map(Some)
    }

    pub(crate) fn retain_jev_performance<T: Serialize + DeserializeOwned>(
        &self,
        run_key: &str,
        receipt: &winwincode_provider::DeviceJevReceipt<T>,
        observed_at: &winwincode_domain::Instant,
    ) -> Result<(), AdapterStoreError> {
        validate_performance_identity(run_key, &receipt.operation_id, observed_at)?;
        let observation = receipt
            .run
            .as_ref()
            .and_then(|run| run.observation.as_ref());
        let input = i64::try_from(observation.map_or(0, |value| value.input_tokens))
            .map_err(|_| AdapterStoreError::Corrupt)?;
        let output = i64::try_from(
            observation
                .and_then(|value| value.output_tokens)
                .unwrap_or(0),
        )
        .map_err(|_| AdapterStoreError::Corrupt)?;
        validate_metric(input)?;
        validate_metric(output)?;
        let usage_known = observation.is_some_and(|value| value.output_tokens.is_some())
            && receipt
                .run
                .as_ref()
                .is_some_and(|run| run.failures.is_empty());
        let serialized = serde_json::to_string(receipt).map_err(|_| AdapterStoreError::Corrupt)?;
        let mut connection = self.lock()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|_| AdapterStoreError::Unavailable)?;
        let previous: Option<(String, bool)> = transaction.query_row(
            "SELECT receipt_json,completed FROM performance_jev_receipt WHERE run_key=?1 AND operation_id=?2",
            params![run_key,receipt.operation_id], |row| Ok((row.get(0)?,row.get(1)?)),
        ).optional().map_err(|_| AdapterStoreError::Unavailable)?;
        if let Some((previous, completed)) = previous {
            if previous == serialized {
                return Ok(());
            }
            let stored: winwincode_provider::DeviceJevReceipt<T> =
                serde_json::from_str(&previous).map_err(|_| AdapterStoreError::Corrupt)?;
            if completed || stored.input_digest != receipt.input_digest || stored.run.is_some() {
                return Err(AdapterStoreError::Conflict);
            }
        }
        transaction.execute(
            "INSERT INTO performance_jev_receipt (run_key,operation_id,receipt_json,completed,input_tokens,output_tokens,usage_known) VALUES (?1,?2,?3,?4,?5,?6,?7)
             ON CONFLICT(run_key,operation_id) DO UPDATE SET receipt_json=excluded.receipt_json,
             completed=excluded.completed,input_tokens=excluded.input_tokens,output_tokens=excluded.output_tokens,usage_known=excluded.usage_known",
            params![run_key,receipt.operation_id,serialized,receipt.run.is_some(),input,output,usage_known],
        ).map_err(|_| AdapterStoreError::Unavailable)?;
        transaction
            .commit()
            .map_err(|_| AdapterStoreError::Unavailable)?;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn close_pending_jev_parent(
        &self,
        run: &str,
        exchange: &str,
        call: &str,
        at: &winwincode_domain::Instant,
    ) -> Result<(), AdapterStoreError> {
        validate_performance_identity(run, exchange, at)?;
        let connection = self.lock()?;
        let completed:bool=connection.query_row("SELECT EXISTS(SELECT 1 FROM performance_operation WHERE run_key=?1 AND operation_kind='primary_model' AND operation_id=?2 AND completed=1)",params![run,call],|r|r.get(0)).map_err(|_|AdapterStoreError::Unavailable)?;
        if !completed {
            return Err(AdapterStoreError::Conflict);
        }
        connection
            .execute(
                "INSERT OR IGNORE INTO performance_jev_parent_terminal VALUES(?1,?2,?3,?4)",
                params![run, exchange, call, at.0],
            )
            .map_err(|_| AdapterStoreError::Unavailable)?;
        Ok(())
    }

    pub(crate) fn delegated_performance_totals(
        &self,
        run_key: &str,
    ) -> Result<DelegatedPerformanceTotals, AdapterStoreError> {
        let connection = self.lock()?;
        connection
            .query_row(
                "SELECT
                   COALESCE(SUM(operation_kind = 'primary_model'), 0),
                   COALESCE(SUM(CASE WHEN operation_kind IN ('primary_model', 'observer', 'jev')
                     THEN input_tokens + cached_tokens + output_tokens ELSE 0 END), 0),
                   COALESCE(SUM(CASE WHEN operation_kind IN ('primary_model', 'observer', 'jev')
                     THEN actual_cost_microunits ELSE 0 END), 0),
                   COALESCE(SUM(CASE WHEN operation_kind = 'primary_model'
                     THEN duration_millis ELSE 0 END), 0),
                   COALESCE(SUM(CASE WHEN operation_kind IN ('primary_model', 'observer', 'jev')
                     AND actual_cost_microunits IS NULL THEN 1 ELSE 0 END), 0)
                   , COALESCE(SUM(CASE WHEN operation_kind IN ('primary_model', 'observer', 'jev')
                     AND completed = 0 THEN 1 ELSE 0 END), 0)
                   , COALESCE(SUM(operation_kind = 'observer'), 0)
                   , COALESCE(SUM(CASE WHEN operation_kind IN ('primary_model','observer','jev') AND usage_known=0 THEN 1 ELSE 0 END),0)
                   , (SELECT COUNT(*) FROM model_call_ledger WHERE run_key=?1
                       AND provider_final=1 AND jev_accounting_complete=0)
                 FROM (
                   SELECT operation_kind,input_tokens,cached_tokens,output_tokens,actual_cost_microunits,duration_millis,
                     CASE WHEN operation_kind='primary_model' AND EXISTS (
                       SELECT 1 FROM model_call_ledger m WHERE m.run_key=p.run_key
                         AND m.model_call_id=p.operation_id AND m.provider_final=1)
                       THEN 1 ELSE completed END AS completed,
                     CASE WHEN completed=0 THEN 0 ELSE usage_known END AS usage_known
                   FROM performance_operation_accounted p WHERE run_key=?1
                     AND NOT (operation_kind='observer' AND EXISTS (
                       SELECT 1 FROM observer_settlement s WHERE s.run_key=p.run_key
                         AND s.batch_id=p.operation_id))
                   UNION ALL
                   -- Observer settlement is a durable receipt, independent of
                   -- the optional operation timing/count projection.
                   SELECT 'observer',COALESCE(json_extract(settlement_json,'$.usage.knownTokens'),0),0,0,
                     json_extract(settlement_json,'$.usage.costMicrounits'),0,1,
                     COALESCE(json_type(settlement_json,'$.usage.tokens')='integer'
                       AND json_extract(settlement_json,'$.usage.accountingStatus')='known',0)
                   FROM observer_settlement WHERE run_key=?1
                   UNION ALL
                   -- Missing telemetry never erases an admitted model identity or
                   -- turns its unknown usage/cost into an available hard budget.
                   SELECT 'primary_model',0,0,0,NULL,0,provider_final,0
                   FROM model_call_ledger m WHERE run_key=?1 AND NOT EXISTS (
                     SELECT 1 FROM performance_operation p WHERE p.run_key=m.run_key
                       AND p.operation_kind='primary_model' AND p.operation_id=m.model_call_id)
                   UNION ALL
                   SELECT 'jev',input_tokens,0,output_tokens,NULL,0,
                     CASE WHEN completed=1 OR EXISTS(SELECT 1 FROM model_call_ledger m
                       WHERE m.run_key=j.run_key AND m.provider_final=1
                       AND (instr(j.operation_id,'jev:'||m.model_exchange_id||':')=1
                         OR instr(j.operation_id,'judge:'||m.model_exchange_id||':')=1))
                       OR EXISTS(SELECT 1 FROM performance_jev_parent_terminal p WHERE p.run_key=j.run_key AND (instr(j.operation_id,'jev:'||p.exchange_id||':')=1 OR instr(j.operation_id,'judge:'||p.exchange_id||':')=1)) THEN 1 ELSE 0 END,
                     -- Recheck the receipt when a legacy statistics backfill could
                     -- not replace its optimistic default. Missing output stays unknown.
                     COALESCE(usage_known
                       AND json_type(receipt_json,'$.run.observation.outputTokens')='integer'
                       AND json_array_length(receipt_json,'$.run.failures')=0,0)
                   FROM performance_jev_receipt j WHERE run_key=?1
                 )",
                params![run_key],
                |row| {
                    let missing_costs = row.get::<_, i64>(4)?;
                    let pending_model_calls = row.get::<_, i64>(5)?;
                    let missing_jev_accounting = row.get::<_, i64>(8)? > 0;
                    Ok(DelegatedPerformanceTotals {
                        primary_model_calls: row.get(0)?,
                        total_tokens: row.get(1)?,
                        total_cost_microunits: row.get(2)?,
                        elapsed_millis: row.get(3)?,
                        cost_complete: missing_costs == 0 && pending_model_calls == 0 && !missing_jev_accounting,
                        usage_complete: row.get::<_,i64>(7)? == 0 && pending_model_calls == 0 && !missing_jev_accounting,
                        pending_model_calls,
                        observer_calls: row.get(6)?,
                    })
                },
            )
            .map_err(|_| AdapterStoreError::Unavailable)
    }

    pub(crate) fn observer_settlement(
        &self,
        run_key: &str,
        batch_id: &str,
    ) -> Result<Option<crate::contract::DelegatedObserverSettlement>, AdapterStoreError> {
        self.lock()?
            .query_row(
                "SELECT settlement_json FROM observer_settlement WHERE run_key=?1 AND batch_id=?2",
                params![run_key, batch_id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(|_| AdapterStoreError::Unavailable)?
            .map(|json| serde_json::from_str(&json).map_err(|_| AdapterStoreError::Corrupt))
            .transpose()
    }

    pub(crate) fn restore_legacy_observer_settlement(
        &self,
        run_key: &str,
        settlement: &crate::contract::DelegatedObserverSettlement,
    ) -> Result<bool, AdapterStoreError> {
        validate_performance_identity(run_key, &settlement.batch_id.0, &settlement.completed_at)?;
        if settlement
            .usage
            .as_ref()
            .is_some_and(|usage| !winwincode_execution_port::usage::valid_usage(usage))
        {
            return Err(AdapterStoreError::Conflict);
        }
        let json = serde_json::to_string(settlement).map_err(|_| AdapterStoreError::Corrupt)?;
        self.transaction(|tx| {
            let matches: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM performance_operation_accounted WHERE run_key=?1 AND operation_kind='observer' AND operation_id=?2 AND completed=1 AND input_tokens=?3 AND cached_tokens=0 AND output_tokens=0 AND actual_cost_microunits IS ?4 AND usage_known=?5)",
                params![run_key,settlement.batch_id.0,settlement.usage.as_ref().map_or(0,|usage|usage.known_tokens),settlement.usage.as_ref().and_then(|usage|usage.cost_microunits),settlement.usage.as_ref().is_some_and(|usage|usage.tokens.is_some())],
                |row| row.get(0)
            ).map_err(|_| AdapterStoreError::Unavailable)?;
            if !matches { return Ok(false); }
            tx.execute("INSERT OR IGNORE INTO observer_settlement(run_key,batch_id,settlement_json) VALUES(?1,?2,?3)",params![run_key,settlement.batch_id.0,json]).map_err(|_|AdapterStoreError::Unavailable)?;
            let saved: String = tx.query_row("SELECT settlement_json FROM observer_settlement WHERE run_key=?1 AND batch_id=?2",params![run_key,settlement.batch_id.0],|row|row.get(0)).map_err(|_|AdapterStoreError::Unavailable)?;
            let saved: crate::contract::DelegatedObserverSettlement = serde_json::from_str(&saved).map_err(|_|AdapterStoreError::Corrupt)?;
            if saved.usage != settlement.usage || saved.batch_id != settlement.batch_id { return Err(AdapterStoreError::Conflict); }
            Ok(true)
        })
    }

    pub(crate) fn retain_observer_settlement(
        &self,
        run_key: &str,
        settlement: &crate::contract::DelegatedObserverSettlement,
    ) -> Result<(), AdapterStoreError> {
        validate_performance_identity(run_key, &settlement.batch_id.0, &settlement.completed_at)?;
        if settlement
            .usage
            .as_ref()
            .is_some_and(|usage| !winwincode_execution_port::usage::valid_usage(usage))
        {
            return Err(AdapterStoreError::Conflict);
        }
        let json = serde_json::to_string(settlement).map_err(|_| AdapterStoreError::Corrupt)?;
        self.transaction(|tx| {
            let previous:Option<String> = tx.query_row("SELECT settlement_json FROM observer_settlement WHERE run_key=?1 AND batch_id=?2",
                params![run_key,settlement.batch_id.0], |row| row.get(0)).optional().map_err(|_| AdapterStoreError::Unavailable)?;
            if let Some(previous) = previous {
                let previous:crate::contract::DelegatedObserverSettlement = serde_json::from_str(&previous).map_err(|_| AdapterStoreError::Corrupt)?;
                if previous.batch_id != settlement.batch_id || previous.usage != settlement.usage { return Err(AdapterStoreError::Conflict); }
                return Ok(());
            }
            tx.execute("INSERT INTO observer_settlement(run_key,batch_id,settlement_json) VALUES(?1,?2,?3)",
                params![run_key,settlement.batch_id.0,json]).map_err(|_| AdapterStoreError::Unavailable)?;
            Ok(())
        })
    }

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
                 FROM performance_operation_accounted WHERE run_key = ?1",
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
                 FROM performance_operation_accounted WHERE run_key = ?1",
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
        let cache_complete:bool=connection.query_row("SELECT NOT EXISTS(SELECT 1 FROM performance_operation_accounted WHERE run_key=?1 AND operation_kind='primary_model' AND cache_breakdown_known=0)",[run_key],|r|r.get(0)).map_err(|_|AdapterStoreError::Unavailable)?;
        let report = PerformanceBaselineReport {
            usage_complete: connection.query_row("SELECT NOT EXISTS(SELECT 1 FROM performance_operation_accounted WHERE run_key=?1 AND operation_kind IN ('primary_model','observer') AND (completed=0 OR usage_known=0))",params![run_key],|row|row.get(0)).map_err(|_|AdapterStoreError::Unavailable)?,
execution_mode,
            observer_mode,
            primary_model_call_count: totals.0,
                primary_model_inclusive_input_tokens:Some(totals.1+totals.2),
            primary_model_input_tokens:cache_complete.then_some(totals.1),
            primary_model_cached_tokens:cache_complete.then_some(totals.2),
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

    pub(crate) fn retain_model_task_context(
        &self,
        run_key: &str,
        call_id: &str,
        request_digest: &Sha256Digest,
        context: &[u8],
    ) -> Result<Vec<u8>, AdapterStoreError> {
        if run_key.trim().is_empty()
            || call_id.trim().is_empty()
            || context.len() > MAX_MODEL_CALL_FRAME_BYTES
        {
            return Err(AdapterStoreError::Conflict);
        }
        let context_digest = format!("sha256:{:x}", Sha256::digest(context));
        self.transaction(|transaction| {
            transaction.execute(
                "INSERT OR IGNORE INTO model_call_task_context VALUES (?1, ?2, ?3, ?4, ?5)",
                params![run_key, call_id, request_digest.0, context_digest, context],
            ).map_err(|_| AdapterStoreError::Unavailable)?;
            let (saved_request, saved_digest, saved): (String, String, Vec<u8>) = transaction.query_row(
                "SELECT request_digest, context_digest, context_text FROM model_call_task_context
                 WHERE run_key=?1 AND model_call_id=?2",
                params![run_key, call_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            ).map_err(|_| AdapterStoreError::Unavailable)?;
            if saved_request != request_digest.0 {
                return Err(AdapterStoreError::Conflict);
            }
            if saved_digest != format!("sha256:{:x}", Sha256::digest(&saved)) {
                return Err(AdapterStoreError::Corrupt);
            }
            Ok(saved)
        })
    }

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
            let sealed: bool = transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM model_call_response_seal WHERE run_key=?1 AND model_call_id=?2)",
                params![run_key,model_call_id], |row|row.get(0),
            ).map_err(|_|AdapterStoreError::Unavailable)?;
            if sealed { return Err(AdapterStoreError::Conflict); }
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

    pub(crate) fn model_call_frame_position(
        &self,
        run_key: &str,
        model_call_id: &str,
        chunk: &ModelChunkMessage,
    ) -> Result<(bool, bool, bool), AdapterStoreError> {
        let connection = self.lock()?;
        let last: Option<(i64,bool)> = connection.query_row(
            "SELECT sequence,json_extract(frame_json,'$.isFinal') FROM model_call_frame WHERE run_key=?1 AND model_call_id=?2 ORDER BY sequence DESC LIMIT 1",
            params![run_key,model_call_id],|row|Ok((row.get(0)?,row.get(1)?))).optional().map_err(|_|AdapterStoreError::Unavailable)?;
        let highest = last.as_ref().map_or(0, |x| x.0);
        let exact = if chunk.sequence.0 <= highest {
            let stored: Option<(String,Vec<u8>)> = connection.query_row(
                "SELECT frame_digest,frame_json FROM model_call_frame WHERE run_key=?1 AND model_call_id=?2 AND sequence=?3",
                params![run_key,model_call_id,chunk.sequence.0],|row|Ok((row.get(0)?,row.get(1)?))).optional().map_err(|_|AdapterStoreError::Unavailable)?;
            if let Some((digest, bytes)) = stored {
                #[cfg(test)]
                crate::audit_model_metrics::record_frame_read(bytes.len());
                if format!("sha256:{:x}", Sha256::digest(&bytes)) != digest {
                    return Err(AdapterStoreError::Corrupt);
                }
                let retained: ModelChunkMessage =
                    serde_json::from_slice(&bytes).map_err(|_| AdapterStoreError::Corrupt)?;
                retained == *chunk
            } else {
                return Err(AdapterStoreError::Corrupt);
            }
        } else {
            false
        };
        Ok((
            exact,
            chunk.sequence.0 == highest.saturating_add(1),
            last.is_some_and(|x| x.1),
        ))
    }

    pub(crate) fn load_model_call_frames(
        &self,
        run_key: &str,
        model_call_id: &str,
    ) -> Result<Vec<ModelChunkMessage>, AdapterStoreError> {
        self.load_model_call_frames_after(run_key, model_call_id, 0)
    }

    pub(crate) fn load_model_call_frames_after(
        &self,
        run_key: &str,
        model_call_id: &str,
        after: u64,
    ) -> Result<Vec<ModelChunkMessage>, AdapterStoreError> {
        if run_key.trim().is_empty() || model_call_id.trim().is_empty() {
            return Err(AdapterStoreError::Conflict);
        }
        let connection = self.lock()?;
        Self::load_model_call_frames_after_in_connection(&connection, run_key, model_call_id, after)
    }

    fn load_model_call_frames_after_in_connection(
        connection: &Connection,
        run_key: &str,
        model_call_id: &str,
        after: u64,
    ) -> Result<Vec<ModelChunkMessage>, AdapterStoreError> {
        let mut statement = connection
            .prepare(
                "SELECT sequence, frame_digest, frame_json
                 FROM model_call_frame
                 WHERE run_key = ?1 AND model_call_id = ?2 AND sequence > ?3
                 ORDER BY sequence",
            )
            .map_err(|_| AdapterStoreError::Unavailable)?;
        let rows = statement
            .query_map(
                params![
                    run_key,
                    model_call_id,
                    i64::try_from(after).map_err(|_| AdapterStoreError::Conflict)?
                ],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Vec<u8>>(2)?,
                    ))
                },
            )
            .map_err(|_| AdapterStoreError::Unavailable)?;
        let mut frames = Vec::new();
        for row in rows {
            let (sequence, expected_digest, frame) =
                row.map_err(|_| AdapterStoreError::Unavailable)?;
            #[cfg(test)]
            crate::audit_model_metrics::record_frame_read(frame.len());
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
            crate::model_bridge::validate_chunk_for_retention(&chunk)
                .map_err(|_| AdapterStoreError::Corrupt)?;
            if chunk.sequence.0 != sequence
                || chunk.sequence.0
                    != i64::try_from(after.saturating_add(frames.len() as u64).saturating_add(1))
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

    pub(crate) fn seal_model_call_response(
        &self,
        run_key: &str,
        model_call_id: &str,
    ) -> Result<(), AdapterStoreError> {
        if run_key.trim().is_empty() || model_call_id.trim().is_empty() {
            return Err(AdapterStoreError::Conflict);
        }
        let mut connection = self.lock()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|_| AdapterStoreError::Unavailable)?;
        let seal: Option<(i64, String)> = transaction.query_row(
            "SELECT frame_count,response_digest FROM model_call_response_seal WHERE run_key=?1 AND model_call_id=?2",
            params![run_key,model_call_id],
            |row|Ok((row.get(0)?,row.get(1)?)),
        ).optional().map_err(|_|AdapterStoreError::Unavailable)?;
        if let Some((count, digest)) = seal {
            let last: Option<(i64, bool)> = transaction.query_row(
                "SELECT sequence,json_extract(frame_json,'$.isFinal') FROM model_call_frame WHERE run_key=?1 AND model_call_id=?2 ORDER BY sequence DESC LIMIT 1",
                params![run_key,model_call_id],
                |row|Ok((row.get(0)?,row.get(1)?)),
            ).optional().map_err(|_|AdapterStoreError::Unavailable)?;
            if count <= 0 || !valid_sha256_digest(&digest) || last != Some((count, true)) {
                return Err(AdapterStoreError::Corrupt);
            }
            return transaction
                .commit()
                .map_err(|_| AdapterStoreError::Unavailable);
        }
        let frames = Self::load_model_call_frames_after_in_connection(
            &transaction,
            run_key,
            model_call_id,
            0,
        )?;
        let first = frames.first().ok_or(AdapterStoreError::Corrupt)?;
        if !frames.last().is_some_and(|frame| frame.is_final) {
            return Err(AdapterStoreError::Corrupt);
        }
        let expected_exchange: Option<String> = transaction.query_row(
            "SELECT model_exchange_id FROM model_call_ledger WHERE run_key=?1 AND model_call_id=?2",
            params![run_key,model_call_id], |row|row.get(0),
        ).optional().map_err(|_|AdapterStoreError::Unavailable)?;
        if expected_exchange.as_deref() != Some(first.model_exchange_id.0.as_str()) {
            return Err(AdapterStoreError::Corrupt);
        }
        let mut digest = Sha256::new();
        for frame in &frames {
            if frame.model_exchange_id != first.model_exchange_id
                || frame.lease != first.lease
                || frame.worker_session_id != first.worker_session_id
                || frame.session_identity != first.session_identity
            {
                return Err(AdapterStoreError::Corrupt);
            }
            let bytes = serde_json::to_vec(frame).map_err(|_| AdapterStoreError::Corrupt)?;
            digest.update((bytes.len() as u64).to_le_bytes());
            digest.update(bytes);
        }
        let count = i64::try_from(frames.len()).map_err(|_| AdapterStoreError::Corrupt)?;
        let digest = format!("sha256:{:x}", digest.finalize());
        let last: i64 = transaction.query_row(
            "SELECT COALESCE(MAX(sequence),0) FROM model_call_frame WHERE run_key=?1 AND model_call_id=?2",
            params![run_key,model_call_id], |row|row.get(0),
        ).map_err(|_|AdapterStoreError::Unavailable)?;
        if last != count {
            return Err(AdapterStoreError::Conflict);
        }
        let changed = transaction.execute(
            "INSERT INTO model_call_response_seal(run_key,model_call_id,frame_count,response_digest) VALUES(?1,?2,?3,?4)",
            params![run_key,model_call_id,count,digest],
        ).map_err(|_|AdapterStoreError::Unavailable)?;
        if changed != 1 {
            return Err(AdapterStoreError::Conflict);
        }
        transaction
            .commit()
            .map_err(|_| AdapterStoreError::Unavailable)
    }

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

    pub(crate) fn mark_model_call_provider_final(
        &self,
        run_key: &str,
        model_call_id: &str,
    ) -> Result<(), AdapterStoreError> {
        self.update_model_call_phase(run_key, model_call_id, false, None)
    }

    pub(crate) fn mark_model_call_provider_final_with_accounting(
        &self,
        run_key: &str,
        model_call_id: &str,
        jev_accounting_complete: bool,
    ) -> Result<(), AdapterStoreError> {
        self.update_model_call_phase(run_key, model_call_id, false, Some(jev_accounting_complete))
    }

    #[cfg(test)]
    pub(crate) fn commit_model_call(
        &self,
        run_key: &str,
        model_call_id: &str,
    ) -> Result<(), AdapterStoreError> {
        self.update_model_call_phase(run_key, model_call_id, true, None)
    }

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
        jev_accounting_complete: Option<bool>,
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
                 SET completed = 1, provider_final = ?3, core_committed = ?4,
                   jev_accounting_complete = COALESCE(?5,jev_accounting_complete)
                 WHERE run_key = ?1 AND model_call_id = ?2",
                params![
                    run_key,
                    model_call_id,
                    new_provider_final,
                    new_core_committed,
                    jev_accounting_complete
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
                       operation_id, turn_id, request_digest, detail_json, resolution_digest, state
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                    params![
                        operation.approval_id,
                        operation.run_key,
                        operation.kernel_session_id,
                        operation.operation_kind.as_str(),
                        operation.operation_id,
                        operation.turn_id,
                        operation.request_digest,
                        serde_json::to_vec(&operation.detail)
                            .map_err(|_| AdapterStoreError::Corrupt)?,
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
                        operation_id, turn_id, request_digest, detail_json, resolution_digest, state
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
                    row.get::<_, Vec<u8>>(7)?,
                    row.get::<_, Option<String>>(8)?,
                    row.get::<_, String>(9)?,
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
                detail_json,
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
                detail: serde_json::from_slice(&detail_json)
                    .map_err(|_| AdapterStoreError::Corrupt)?,
                resolution_digest,
                state: StoredApprovalOperationState::parse(&state)?,
            };
            operation.validate()?;
            Ok(operation)
        })
        .collect()
    }

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

    pub(crate) fn retain_input_call<T: Serialize + DeserializeOwned>(
        &self,
        run_key: &str,
        request_digest: &str,
        record: &T,
    ) -> Result<T, AdapterStoreError> {
        validate_digest(request_digest)?;
        if run_key.is_empty() {
            return Err(AdapterStoreError::Conflict);
        }
        let bytes = serde_json::to_vec(record).map_err(|_| AdapterStoreError::Corrupt)?;
        self.transaction(|transaction| {
            transaction
                .execute(
                    "INSERT OR IGNORE INTO input_call(run_key, request_digest, record_json)
                 VALUES (?1, ?2, ?3)",
                    params![run_key, request_digest, bytes],
                )
                .map_err(|_| AdapterStoreError::Unavailable)?;
            let stored: Vec<u8> = transaction
                .query_row(
                    "SELECT record_json FROM input_call WHERE run_key=?1 AND request_digest=?2",
                    params![run_key, request_digest],
                    |row| row.get(0),
                )
                .map_err(|_| AdapterStoreError::Unavailable)?;
            serde_json::from_slice(&stored).map_err(|_| AdapterStoreError::Corrupt)
        })
    }

    pub(crate) fn load_input_call<T: DeserializeOwned>(
        &self,
        run_key: &str,
        request_digest: &str,
    ) -> Result<Option<T>, AdapterStoreError> {
        let bytes: Option<Vec<u8>> = self
            .lock()?
            .query_row(
                "SELECT record_json FROM input_call WHERE run_key=?1 AND request_digest=?2",
                params![run_key, request_digest],
                |row| row.get(0),
            )
            .optional()
            .map_err(|_| AdapterStoreError::Unavailable)?;
        bytes
            .map(|bytes| serde_json::from_slice(&bytes).map_err(|_| AdapterStoreError::Corrupt))
            .transpose()
    }

    pub(crate) fn replace_input_call<T: Serialize>(
        &self,
        run_key: &str,
        request_digest: &str,
        previous: &T,
        next: &T,
    ) -> Result<(), AdapterStoreError> {
        let previous = serde_json::to_vec(previous).map_err(|_| AdapterStoreError::Corrupt)?;
        let next = serde_json::to_vec(next).map_err(|_| AdapterStoreError::Corrupt)?;
        let changed = self
            .lock()?
            .execute(
                "UPDATE input_call SET record_json=?4
             WHERE run_key=?1 AND request_digest=?2 AND record_json=?3",
                params![run_key, request_digest, previous, next],
            )
            .map_err(|_| AdapterStoreError::Unavailable)?;
        if changed != 1 {
            return Err(AdapterStoreError::Conflict);
        }
        Ok(())
    }

    pub(crate) fn resolve_input_call_answer<T: Serialize>(
        &self,
        operation: &StoredInputOperation,
        resolution_digest: &str,
        previous: &T,
        next: &T,
    ) -> Result<(), AdapterStoreError> {
        operation.validate()?;
        validate_digest(resolution_digest)?;
        let previous = serde_json::to_vec(previous).map_err(|_| AdapterStoreError::Corrupt)?;
        let next = serde_json::to_vec(next).map_err(|_| AdapterStoreError::Corrupt)?;
        self.transaction(|transaction| {
            if load_input_operation(transaction, &operation.input_request_id)?.as_ref()
                != Some(operation)
            {
                return Err(AdapterStoreError::Conflict);
            }
            let changed = transaction
                .execute(
                    "UPDATE input_call SET record_json=?4
                     WHERE run_key=?1 AND request_digest=?2 AND record_json=?3",
                    params![operation.run_key, operation.request_digest, previous, next],
                )
                .map_err(|_| AdapterStoreError::Unavailable)?;
            if changed != 1 {
                return Err(AdapterStoreError::Conflict);
            }
            settle_input_operation(
                transaction,
                &operation.input_request_id,
                &operation.request_digest,
                resolution_digest,
            )?;
            Ok(())
        })
    }

    pub(crate) fn retain_input_operation(
        &self,
        operation: &StoredInputOperation,
    ) -> Result<StoredInputOperation, AdapterStoreError> {
        operation.validate()?;
        let choice_identities_json = serde_json::to_vec(&operation.choice_identities)
            .map_err(|_| AdapterStoreError::Corrupt)?;
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
                       request_digest, choice_identities_json, resolution_digest, state
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                    params![
                        operation.input_request_id,
                        operation.run_key,
                        operation.kernel_session_id,
                        operation.question_id,
                        operation.turn_id,
                        operation.request_digest,
                        choice_identities_json,
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
            settle_input_operation(
                transaction,
                input_request_id,
                request_digest,
                resolution_digest,
            )
        })
    }

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
            if extend_existing_model_lineage(
                transaction,
                run_key,
                existing_root.as_ref(),
                authority_json,
            )? {
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
        report.primary_model_input_tokens.unwrap_or(0),
        report.primary_model_cached_tokens.unwrap_or(0),
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
    let mut has_jev_accounting_complete = false;
    for column in columns {
        match column.map_err(|_| AdapterStoreError::Unavailable)?.as_str() {
            "provider_final" => has_provider_final = true,
            "core_committed" => has_core_committed = true,
            "model_exchange_id" => has_model_exchange_id = true,
            "jev_accounting_complete" => has_jev_accounting_complete = true,
            _ => {}
        }
    }
    drop(statement);
    if !has_jev_accounting_complete {
        connection.execute("ALTER TABLE model_call_ledger ADD COLUMN jev_accounting_complete INTEGER NOT NULL DEFAULT 0 CHECK(jev_accounting_complete IN (0,1))", [])
            .map_err(|_| AdapterStoreError::Unavailable)?;
    }
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

fn ensure_input_operation_choice_identities_column(
    connection: &Connection,
) -> Result<(), AdapterStoreError> {
    let mut statement = connection
        .prepare("PRAGMA table_info(input_operation)")
        .map_err(|_| AdapterStoreError::Unavailable)?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|_| AdapterStoreError::Unavailable)?;
    let mut has_choice_identities = false;
    for column in columns {
        if column.map_err(|_| AdapterStoreError::Unavailable)? == "choice_identities_json" {
            has_choice_identities = true;
        }
    }
    drop(statement);
    if !has_choice_identities {
        connection
            .execute(
                "ALTER TABLE input_operation
                 ADD COLUMN choice_identities_json BLOB NOT NULL DEFAULT X'5B5D'",
                [],
            )
            .map_err(|_| AdapterStoreError::Unavailable)?;
    }
    Ok(())
}

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
    Mcp,
}

impl StoredApprovalOperationKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Exec => "exec",
            Self::Patch => "patch",
            Self::Mcp => "mcp",
        }
    }

    fn parse(value: &str) -> Result<Self, AdapterStoreError> {
        match value {
            "exec" => Ok(Self::Exec),
            "patch" => Ok(Self::Patch),
            "mcp" => Ok(Self::Mcp),
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

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct StoredApprovalOperation {
    pub(crate) approval_id: String,
    pub(crate) run_key: String,
    pub(crate) kernel_session_id: String,
    pub(crate) operation_kind: StoredApprovalOperationKind,
    pub(crate) operation_id: String,
    pub(crate) turn_id: Option<String>,
    pub(crate) request_digest: String,
    pub(crate) detail: Option<ApprovalActionSanitizedDetail>,
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
            || self.detail.as_ref().is_some_and(|detail| {
                detail.request_sha256.0 != self.request_digest
                    || detail.target_count
                        < i64::try_from(detail.target_summaries.len()).unwrap_or(i64::MAX)
            })
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StoredInputOperation {
    pub(crate) input_request_id: String,
    pub(crate) run_key: String,
    pub(crate) kernel_session_id: String,
    pub(crate) question_id: String,
    pub(crate) turn_id: String,
    pub(crate) request_digest: String,
    pub(crate) choice_identities: Vec<StoredInputChoiceIdentity>,
    pub(crate) resolution_digest: Option<String>,
    pub(crate) state: StoredInputOperationState,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct StoredInputChoiceIdentity {
    pub(crate) public_label: String,
    pub(crate) public_occurrence: u64,
    pub(crate) choice_id: String,
}

impl StoredInputOperation {
    fn validate(&self) -> Result<(), AdapterStoreError> {
        let mut replay_keys = HashSet::with_capacity(self.choice_identities.len());
        let mut choice_ids = HashSet::with_capacity(self.choice_identities.len());
        let invalid_choice_identity = self.choice_identities.iter().any(|identity| {
            identity.public_label.trim().is_empty()
                || !canonical_input_choice_id(&identity.choice_id)
                || !replay_keys.insert((identity.public_label.as_str(), identity.public_occurrence))
                || !choice_ids.insert(identity.choice_id.as_str())
        });
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
            || invalid_choice_identity
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

fn canonical_input_choice_id(value: &str) -> bool {
    value.strip_prefix("ich_").is_some_and(|identifier| {
        identifier.len() == 26
            && identifier.bytes().all(|byte| {
                byte.is_ascii_digit()
                    || matches!(
                        byte,
                        b'A'..=b'H' | b'J'..=b'K' | b'M'..=b'N' | b'P'..=b'T' | b'V'..=b'Z'
                    )
            })
    })
}

fn load_approval_operation(
    connection: &Connection,
    approval_id: &str,
) -> Result<Option<StoredApprovalOperation>, AdapterStoreError> {
    let stored = connection
        .query_row(
            "SELECT run_key, kernel_session_id, operation_kind, operation_id,
                    turn_id, request_digest, detail_json, resolution_digest, state
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
                    row.get::<_, Vec<u8>>(6)?,
                    row.get::<_, Option<String>>(7)?,
                    row.get::<_, String>(8)?,
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
                detail_json,
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
                    detail: serde_json::from_slice(&detail_json)
                        .map_err(|_| AdapterStoreError::Corrupt)?,
                    resolution_digest,
                    state: StoredApprovalOperationState::parse(&state)?,
                };
                operation.validate()?;
                Ok(operation)
            },
        )
        .transpose()
}

fn settle_input_operation(
    transaction: &Transaction<'_>,
    input_request_id: &str,
    request_digest: &str,
    resolution_digest: &str,
) -> Result<StoredInputOperation, AdapterStoreError> {
    let mut operation =
        load_input_operation(transaction, input_request_id)?.ok_or(AdapterStoreError::Conflict)?;
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
}

fn load_input_operation(
    connection: &Connection,
    input_request_id: &str,
) -> Result<Option<StoredInputOperation>, AdapterStoreError> {
    let stored = connection
        .query_row(
            "SELECT run_key, kernel_session_id, question_id, turn_id,
                    request_digest, choice_identities_json, resolution_digest, state
             FROM input_operation WHERE input_request_id = ?1",
            params![input_request_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, Vec<u8>>(5)?,
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
                question_id,
                turn_id,
                request_digest,
                choice_identities_json,
                resolution_digest,
                state,
            )| {
                let choice_identities = serde_json::from_slice(&choice_identities_json)
                    .map_err(|_| AdapterStoreError::Corrupt)?;
                let operation = StoredInputOperation {
                    input_request_id: input_request_id.to_owned(),
                    run_key,
                    kernel_session_id,
                    question_id,
                    turn_id,
                    request_digest,
                    choice_identities,
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

    fn load_head(&mut self, stream: &ReplayStreamKey) -> Result<ModelCursorHead, Self::Error> {
        let connection = self.lock()?;
        Ok(load_model_cursor_head(&connection, stream)?.unwrap_or_default())
    }

    fn load_fingerprint(
        &mut self,
        stream: &ReplayStreamKey,
        sequence: u64,
    ) -> Result<Option<ModelChunkFingerprint>, Self::Error> {
        let connection = self.lock()?;
        load_model_fingerprint(&connection, stream, sequence)
    }

    fn record_delivery(
        &mut self,
        stream: &ReplayStreamKey,
        expected_sequence: u64,
        fingerprint: &ModelChunkFingerprint,
        termination: Option<ModelTerminationReason>,
    ) -> Result<(), Self::Error> {
        self.transaction(|transaction| {
            let mut head = load_model_cursor_head(transaction, stream)?.unwrap_or_default();
            if head.confirmed_sequence != expected_sequence || head.termination.is_some() || head.cancellation.is_some()
                || fingerprint.sequence != expected_sequence.checked_add(1).ok_or(AdapterStoreError::Conflict)? {
                return Err(AdapterStoreError::Conflict);
            }
            validate_model_fingerprint(fingerprint)?;
            if termination != (if fingerprint.is_final { Some(if fingerprint.has_error { ModelTerminationReason::ProviderError } else { ModelTerminationReason::Completed }) } else { None }) {
                return Err(AdapterStoreError::Conflict);
            }
            head.confirmed_sequence = fingerprint.sequence;
            head.termination = termination;
            write_model_cursor(transaction, stream, &head)?;
            transaction.execute("INSERT INTO model_cursor_frame(stream_key,sequence,fingerprint_json) VALUES(?1,?2,?3)",
                params![stream.as_str(),i64::try_from(fingerprint.sequence).map_err(|_|AdapterStoreError::Corrupt)?,serde_json::to_vec(fingerprint).map_err(|_|AdapterStoreError::Corrupt)?])
                .map_err(|_|AdapterStoreError::Unavailable)?;
            Ok(())
        })
    }

    fn terminate(
        &mut self,
        stream: &ReplayStreamKey,
        expected_sequence: u64,
        reason: ModelTerminationReason,
    ) -> Result<(), Self::Error> {
        self.transaction(|transaction| {
            let mut head = load_model_cursor_head(transaction, stream)?.unwrap_or_default();
            if head.confirmed_sequence != expected_sequence
                || head.termination.is_some()
                || head.cancellation.is_some()
            {
                return Err(AdapterStoreError::Conflict);
            }
            head.termination = Some(reason);
            write_model_cursor(transaction, stream, &head)
        })
    }

    fn record_cancellation_intent(
        &mut self,
        stream: &ReplayStreamKey,
        expected_sequence: u64,
        fingerprint: &ModelCancellationFingerprint,
    ) -> Result<(), Self::Error> {
        self.transaction(|transaction| {
            let mut head = load_model_cursor_head(transaction, stream)?.unwrap_or_default();
            if head.confirmed_sequence != expected_sequence
                || head.termination.is_some()
                || head.cancellation.is_some()
                || fingerprint.phase != ModelCancellationPhase::Intent
                || !valid_cancellation(fingerprint, expected_sequence)
            {
                return Err(AdapterStoreError::Conflict);
            }
            head.cancellation = Some(fingerprint.clone());
            write_model_cursor(transaction, stream, &head)
        })
    }

    fn complete_cancellation(
        &mut self,
        stream: &ReplayStreamKey,
        expected_sequence: u64,
        fingerprint: &ModelCancellationFingerprint,
    ) -> Result<(), Self::Error> {
        self.transaction(|transaction| {
            let mut head = load_model_cursor_head(transaction, stream)?.unwrap_or_default();
            if head.confirmed_sequence != expected_sequence
                || head.termination.is_some()
                || head.cancellation.as_ref() != Some(fingerprint)
                || fingerprint.phase != ModelCancellationPhase::Intent
            {
                return Err(AdapterStoreError::Conflict);
            }
            let mut applied = fingerprint.clone();
            applied.phase = ModelCancellationPhase::Applied;
            head.cancellation = Some(applied);
            head.termination = Some(ModelTerminationReason::Cancelled);
            write_model_cursor(transaction, stream, &head)
        })
    }
}

fn validate_model_fingerprint(frame: &ModelChunkFingerprint) -> Result<(), AdapterStoreError> {
    let digest = &frame.digest.0;
    if frame.sequence == 0
        || frame.sequence > MAX_SAFE_METRIC as u64
        || frame.message_id.0.is_empty()
        || digest.len() != 71
        || !digest.starts_with("sha256:")
        || !digest[7..]
            .bytes()
            .all(|x| x.is_ascii_digit() || (b'a'..=b'f').contains(&x))
        || frame.has_error && !frame.is_final
    {
        return Err(AdapterStoreError::Corrupt);
    }
    Ok(())
}

fn load_model_fingerprint(
    connection: &Connection,
    stream: &ReplayStreamKey,
    sequence: u64,
) -> Result<Option<ModelChunkFingerprint>, AdapterStoreError> {
    let bytes = connection
        .query_row(
            "SELECT fingerprint_json FROM model_cursor_frame WHERE stream_key=?1 AND sequence=?2",
            params![
                stream.as_str(),
                i64::try_from(sequence).map_err(|_| AdapterStoreError::Corrupt)?
            ],
            |row| row.get::<_, Vec<u8>>(0),
        )
        .optional()
        .map_err(|_| AdapterStoreError::Unavailable)?;
    bytes
        .map(|bytes| {
            #[cfg(test)]
            crate::audit_model_metrics::record_fingerprint_visit();
            let frame: ModelChunkFingerprint =
                serde_json::from_slice(&bytes).map_err(|_| AdapterStoreError::Corrupt)?;
            validate_model_fingerprint(&frame)?;
            if frame.sequence != sequence {
                return Err(AdapterStoreError::Corrupt);
            }
            Ok(frame)
        })
        .transpose()
}

fn load_model_cursor_head(
    connection: &Connection,
    stream: &ReplayStreamKey,
) -> Result<Option<ModelCursorHead>, AdapterStoreError> {
    let head: Option<ModelCursorHead> = load_snapshot(connection, "model_cursor", stream.as_str())?;
    if let Some(head) = &head {
        if head.confirmed_sequence > MAX_SAFE_METRIC as u64 {
            return Err(AdapterStoreError::Corrupt);
        }
        let last = if head.confirmed_sequence > 0 {
            Some(
                load_model_fingerprint(connection, stream, head.confirmed_sequence)?
                    .ok_or(AdapterStoreError::Corrupt)?,
            )
        } else {
            None
        };
        let provider_final = last.as_ref().filter(|f| f.is_final).map(|f| {
            if f.has_error {
                ModelTerminationReason::ProviderError
            } else {
                ModelTerminationReason::Completed
            }
        });
        if (provider_final.is_some()
            || matches!(
                head.termination,
                Some(ModelTerminationReason::Completed | ModelTerminationReason::ProviderError)
            ))
            && head.termination != provider_final
            || head.termination == Some(ModelTerminationReason::Cancelled)
                && !head
                    .cancellation
                    .as_ref()
                    .is_some_and(|x| x.phase == ModelCancellationPhase::Applied)
            || head.cancellation.as_ref().is_some_and(|x| {
                !valid_cancellation(x, head.confirmed_sequence)
                    || (head.termination.is_none() && x.phase != ModelCancellationPhase::Intent)
                    || (head.termination != Some(ModelTerminationReason::Cancelled)
                        && head.termination.is_some())
            })
        {
            return Err(AdapterStoreError::Corrupt);
        }
    }
    Ok(head)
}

fn load_model_cursor(
    connection: &Connection,
    stream: &ReplayStreamKey,
) -> Result<Option<ModelCursorSnapshot>, AdapterStoreError> {
    let Some(head) = load_model_cursor_head(connection, stream)? else {
        return Ok(None);
    };
    let mut snapshot = ModelCursorSnapshot {
        confirmed_sequence: head.confirmed_sequence,
        cancellation: head.cancellation,
        termination: head.termination,
        frames: Vec::new(),
    };
    let mut statement = connection.prepare("SELECT sequence,fingerprint_json FROM model_cursor_frame WHERE stream_key=?1 ORDER BY sequence").map_err(|_|AdapterStoreError::Unavailable)?;
    let rows = statement
        .query_map([stream.as_str()], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?))
        })
        .map_err(|_| AdapterStoreError::Unavailable)?;
    for row in rows {
        let (sequence, bytes) = row.map_err(|_| AdapterStoreError::Unavailable)?;
        let frame: ModelChunkFingerprint =
            serde_json::from_slice(&bytes).map_err(|_| AdapterStoreError::Corrupt)?;
        if frame.sequence != u64::try_from(sequence).map_err(|_| AdapterStoreError::Corrupt)? {
            return Err(AdapterStoreError::Corrupt);
        }
        snapshot.frames.push(frame);
    }
    snapshot
        .validate()
        .map_err(|_| AdapterStoreError::Corrupt)?;
    Ok(Some(snapshot))
}

fn write_model_cursor(
    transaction: &Transaction<'_>,
    stream: &ReplayStreamKey,
    head: &ModelCursorHead,
) -> Result<(), AdapterStoreError> {
    write_snapshot(transaction, "model_cursor", stream.as_str(), head)
}

fn migrate_model_cursor_schema(connection: &Connection) -> Result<(), AdapterStoreError> {
    let transaction = connection
        .unchecked_transaction()
        .map_err(|_| AdapterStoreError::Unavailable)?;
    let done = transaction
        .query_row(
            "SELECT 1 FROM codex_store_migration WHERE migration_key='model-cursor-frames-v1'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .optional()
        .map_err(|_| AdapterStoreError::Unavailable)?;
    if done.is_none() {
        let records = {
            let mut statement = transaction
                .prepare("SELECT stream_key,snapshot_json FROM model_cursor")
                .map_err(|_| AdapterStoreError::Unavailable)?;
            statement
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
                })
                .map_err(|_| AdapterStoreError::Unavailable)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|_| AdapterStoreError::Unavailable)?
        };
        for (stream, bytes) in records {
            let snapshot: ModelCursorSnapshot =
                serde_json::from_slice(&bytes).map_err(|_| AdapterStoreError::Corrupt)?;
            snapshot
                .validate()
                .map_err(|_| AdapterStoreError::Corrupt)?;
            for frame in &snapshot.frames {
                transaction.execute("INSERT INTO model_cursor_frame(stream_key,sequence,fingerprint_json) VALUES(?1,?2,?3)",params![stream,i64::try_from(frame.sequence).map_err(|_|AdapterStoreError::Corrupt)?,serde_json::to_vec(frame).map_err(|_|AdapterStoreError::Corrupt)?]).map_err(|_|AdapterStoreError::Unavailable)?;
            }
            let head: ModelCursorHead = snapshot.into();
            transaction
                .execute(
                    "UPDATE model_cursor SET snapshot_json=?1 WHERE stream_key=?2",
                    params![
                        serde_json::to_vec(&head).map_err(|_| AdapterStoreError::Corrupt)?,
                        stream
                    ],
                )
                .map_err(|_| AdapterStoreError::Unavailable)?;
        }
        transaction
            .execute(
                "INSERT INTO codex_store_migration(migration_key) VALUES('model-cursor-frames-v1')",
                [],
            )
            .map_err(|_| AdapterStoreError::Unavailable)?;
    }
    transaction
        .commit()
        .map_err(|_| AdapterStoreError::Unavailable)
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

fn ensure_runtime_replay(
    tx: &Transaction<'_>,
    stream: &ReplayStreamKey,
) -> Result<bool, AdapterStoreError> {
    if tx
        .query_row(
            "SELECT 1 FROM runtime_replay_head WHERE stream_key=?1",
            [stream.as_str()],
            |_| Ok(()),
        )
        .optional()
        .map_err(|_| AdapterStoreError::Unavailable)?
        .is_some()
    {
        return Ok(true);
    }
    let old: Option<StoredReplaySnapshot> = load_snapshot(tx, "runtime_replay", stream.as_str())?;
    let Some(old) = old else {
        return Ok(false);
    };
    let snapshot = ReplaySnapshot::from(old);
    snapshot
        .validate()
        .map_err(|_| AdapterStoreError::Corrupt)?;
    tx.execute(
        "INSERT INTO runtime_replay_head VALUES (?1,?2,?3)",
        params![
            stream.as_str(),
            replay_integer(snapshot.ack_sequence)?,
            replay_integer(snapshot.highest_sequence)?
        ],
    )
    .map_err(|_| AdapterStoreError::Unavailable)?;
    for frame in &snapshot.events {
        insert_runtime_frame(tx, stream, frame)?;
    }
    if snapshot.events.is_empty() && snapshot.highest_sequence > 0 {
        tx.execute(
            "INSERT INTO runtime_replay_legacy_compaction VALUES (?1,?2)",
            params![stream.as_str(), replay_integer(snapshot.highest_sequence)?],
        )
        .map_err(|_| AdapterStoreError::Unavailable)?;
    }
    tx.execute(
        "DELETE FROM runtime_replay WHERE stream_key=?1",
        [stream.as_str()],
    )
    .map_err(|_| AdapterStoreError::Unavailable)?;
    Ok(true)
}

fn replay_integer(value: u64) -> Result<i64, AdapterStoreError> {
    i64::try_from(value).map_err(|_| AdapterStoreError::Corrupt)
}

fn insert_runtime_frame(
    tx: &Transaction<'_>,
    stream: &ReplayStreamKey,
    frame: &ReplayFrame,
) -> Result<(), AdapterStoreError> {
    if frame.sequence == 0 || frame.event_id.is_empty() || frame.digest.is_empty() {
        return Err(AdapterStoreError::Corrupt);
    }
    tx.execute(
        "INSERT INTO runtime_replay_frame VALUES (?1,?2,?3,?4,?5)",
        params![
            stream.as_str(),
            replay_integer(frame.sequence)?,
            frame.event_id,
            frame.digest,
            frame.frame
        ],
    )
    .map_err(|_| AdapterStoreError::Conflict)?;
    #[cfg(test)]
    crate::storage_mechanism_regression::snapshot_write(
        "runtime_replay",
        frame.frame.len() + frame.event_id.len() + frame.digest.len() + 8,
    );
    Ok(())
}

fn read_runtime_head(
    tx: &Transaction<'_>,
    stream: &ReplayStreamKey,
) -> Result<ReplaySnapshot, AdapterStoreError> {
    let head: Option<(i64, i64)> = tx
        .query_row(
            "SELECT ack_sequence,highest_sequence FROM runtime_replay_head WHERE stream_key=?1",
            [stream.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(|_| AdapterStoreError::Unavailable)?;
    let Some((ack, highest)) = head else {
        return Ok(ReplaySnapshot::default());
    };
    if ack < 0 || highest < ack {
        return Err(AdapterStoreError::Corrupt);
    }
    let tail: Option<(i64, String, String)> = tx.query_row("SELECT sequence,event_id,digest FROM runtime_replay_frame WHERE stream_key=?1 ORDER BY sequence DESC LIMIT 1", [stream.as_str()], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?))).optional().map_err(|_| AdapterStoreError::Unavailable)?;
    let tail_bytes = if let Some((sequence, event_id, digest)) = tail {
        if sequence <= 0 || sequence != highest || event_id.is_empty() || digest.is_empty() {
            return Err(AdapterStoreError::Corrupt);
        }
        event_id.len() + digest.len() + 8
    } else {
        let compacted: Option<i64> = tx.query_row("SELECT compacted_through FROM runtime_replay_legacy_compaction WHERE stream_key=?1", [stream.as_str()], |row| row.get(0)).optional().map_err(|_| AdapterStoreError::Unavailable)?;
        if ack != highest || (highest > 0 && compacted != Some(highest)) {
            return Err(AdapterStoreError::Corrupt);
        }
        0
    };
    #[cfg(test)]
    crate::storage_mechanism_regression::snapshot_read("runtime_replay", 16 + tail_bytes);
    #[cfg(not(test))]
    let _ = tail_bytes;
    Ok(ReplaySnapshot {
        ack_sequence: u64::try_from(ack).map_err(|_| AdapterStoreError::Corrupt)?,
        highest_sequence: u64::try_from(highest).map_err(|_| AdapterStoreError::Corrupt)?,
        events: Vec::new(),
    })
}

fn read_runtime_frames(
    tx: &Transaction<'_>,
    stream: &ReplayStreamKey,
    query: ReplayReadQuery<'_>,
) -> Result<ReplaySnapshot, AdapterStoreError> {
    ensure_runtime_replay(tx, stream)?;
    let mut snapshot = read_runtime_head(tx, stream)?;
    let (sql, id, sequence, limit) = match query {
        ReplayReadQuery::Watermarks => return Ok(snapshot),
        ReplayReadQuery::Conflict { event_id, sequence } => (
            "SELECT event_id,sequence,digest,frame FROM runtime_replay_frame WHERE stream_key=?1 AND event_id=?2 UNION ALL SELECT event_id,sequence,digest,frame FROM runtime_replay_frame WHERE stream_key=?1 AND sequence=?3 AND event_id<>?2 ORDER BY sequence LIMIT ?4",
            event_id,
            replay_integer(sequence)?,
            2_i64,
        ),
        ReplayReadQuery::After {
            sequence,
            max_events,
        } => (
            "SELECT event_id,sequence,digest,frame FROM runtime_replay_frame WHERE stream_key=?1 AND sequence>?3 ORDER BY sequence LIMIT ?4",
            "",
            replay_integer(sequence)?,
            i64::try_from(max_events).unwrap_or(i64::MAX),
        ),
    };
    let mut stmt = tx
        .prepare(sql)
        .map_err(|_| AdapterStoreError::Unavailable)?;
    let rows = stmt
        .query_map(params![stream.as_str(), id, sequence, limit], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Vec<u8>>(3)?,
            ))
        })
        .map_err(|_| AdapterStoreError::Unavailable)?;
    for row in rows {
        let (event_id, sequence, digest, frame) =
            row.map_err(|_| AdapterStoreError::Unavailable)?;
        if sequence <= 0
            || event_id.is_empty()
            || digest.is_empty()
            || u64::try_from(sequence).map_err(|_| AdapterStoreError::Corrupt)?
                > snapshot.highest_sequence
        {
            return Err(AdapterStoreError::Corrupt);
        }
        #[cfg(test)]
        crate::storage_mechanism_regression::snapshot_read(
            "runtime_replay",
            frame.len() + event_id.len() + digest.len() + 8,
        );
        snapshot.events.push(ReplayFrame::new(
            event_id,
            u64::try_from(sequence).map_err(|_| AdapterStoreError::Corrupt)?,
            digest,
            frame,
        ));
    }
    #[cfg(test)]
    crate::storage_mechanism_regression::replay_query(
        stmt.get_status(rusqlite::StatementStatus::VmStep),
    );
    if let ReplayReadQuery::After { sequence, .. } = query {
        let mut expected = snapshot
            .events
            .first()
            .map_or(sequence.saturating_add(1), |frame| frame.sequence);
        if expected > sequence.saturating_add(1)
            && expected > snapshot.ack_sequence.saturating_add(1)
        {
            return Err(AdapterStoreError::Corrupt);
        }
        for frame in &snapshot.events {
            if frame.sequence != expected {
                return Err(AdapterStoreError::Corrupt);
            }
            expected = expected.checked_add(1).ok_or(AdapterStoreError::Corrupt)?;
        }
        if sequence < snapshot.highest_sequence
            && snapshot.events.is_empty()
            && snapshot.highest_sequence > snapshot.ack_sequence
        {
            return Err(AdapterStoreError::Corrupt);
        }
    }
    Ok(snapshot)
}

impl ReplayStore for AdapterStore {
    type Error = AdapterStoreError;
    fn load(&mut self, stream: &ReplayStreamKey) -> Result<Option<ReplaySnapshot>, Self::Error> {
        self.transaction(|tx| {
            if !ensure_runtime_replay(tx, stream)? {
                return Ok(None);
            }
            let snapshot = read_runtime_frames(
                tx,
                stream,
                ReplayReadQuery::After {
                    sequence: 0,
                    max_events: usize::MAX,
                },
            )?;
            snapshot
                .validate()
                .map_err(|_| AdapterStoreError::Corrupt)?;
            Ok(Some(snapshot))
        })
    }
    fn load_indexed(
        &mut self,
        stream: &ReplayStreamKey,
        query: ReplayReadQuery<'_>,
    ) -> Result<Option<ReplaySnapshot>, Self::Error> {
        self.transaction(|tx| read_runtime_frames(tx, stream, query).map(Some))
    }
    fn append(
        &mut self,
        stream: &ReplayStreamKey,
        expected_highest_sequence: u64,
        frame: &ReplayFrame,
    ) -> Result<(), Self::Error> {
        self.transaction(|tx| {
            if !ensure_runtime_replay(tx, stream)? {
                tx.execute(
                    "INSERT INTO runtime_replay_head VALUES (?1,0,0)",
                    [stream.as_str()],
                )
                .map_err(|_| AdapterStoreError::Unavailable)?;
            }
            let snapshot = read_runtime_head(tx, stream)?;
            if snapshot.highest_sequence != expected_highest_sequence
                || Some(frame.sequence) != expected_highest_sequence.checked_add(1)
            {
                return Err(AdapterStoreError::Conflict);
            }
            insert_runtime_frame(tx, stream, frame)?;
            tx.execute(
                "UPDATE runtime_replay_head SET highest_sequence=?2 WHERE stream_key=?1",
                params![stream.as_str(), replay_integer(frame.sequence)?],
            )
            .map_err(|_| AdapterStoreError::Unavailable)?;
            #[cfg(test)]
            crate::storage_mechanism_regression::snapshot_write("runtime_replay", 8);
            Ok(())
        })
    }
}

impl ReplayAcknowledgementStore for AdapterStore {
    fn record_acknowledgement(
        &mut self,
        stream: &ReplayStreamKey,
        expected_ack_sequence: u64,
        ack_sequence: u64,
    ) -> Result<(), Self::Error> {
        self.transaction(|tx| {
            ensure_runtime_replay(tx, stream)?;
            let snapshot = read_runtime_head(tx, stream)?;
            if snapshot.ack_sequence != expected_ack_sequence
                || ack_sequence < expected_ack_sequence
                || ack_sequence > snapshot.highest_sequence
            {
                return Err(AdapterStoreError::Conflict);
            }
            tx.execute(
                "UPDATE runtime_replay_head SET ack_sequence=?2 WHERE stream_key=?1",
                params![stream.as_str(), replay_integer(ack_sequence)?],
            )
            .map_err(|_| AdapterStoreError::Unavailable)?;
            #[cfg(test)]
            crate::storage_mechanism_regression::snapshot_write("runtime_replay", 8);
            Ok(())
        })
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
    #[cfg(test)]
    if table == "model_cursor"
        && let Some(bytes) = &bytes
    {
        crate::audit_model_metrics::record_cursor_read(bytes.len());
    }
    #[cfg(test)]
    crate::storage_mechanism_regression::snapshot_read(table, bytes.as_ref().map_or(0, Vec::len));
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
    #[cfg(test)]
    let serialized_len = bytes.len();
    transaction
        .execute(&statement, params![key, bytes])
        .map_err(|_| AdapterStoreError::Unavailable)?;
    #[cfg(test)]
    if table == "model_cursor" {
        crate::audit_model_metrics::record_cursor_write(serialized_len);
    }
    #[cfg(test)]
    crate::storage_mechanism_regression::snapshot_write(table, serialized_len);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        AdapterStore, AdapterStoreError, DATABASE_FILE, ModelCallPhase, StoredApprovalOperation,
        StoredApprovalOperationKind, StoredApprovalOperationState, StoredInputChoiceIdentity,
        StoredInputOperation, StoredInputOperationState,
    };
    use crate::performance::{PerformanceOperationCompletion, PerformanceOperationKind};
    use std::path::{Path, PathBuf};
    use winwincode_domain::Sha256Digest;
    use winwincode_domain::{ExecutionEventId, ExecutionSequence, Instant, ModelExchangeId};
    use winwincode_execution_port::runtime_trace_outbox::{
        ExecutionMode, ObserverMode, PerformanceBaselineReport,
    };
    use winwincode_provider::{DeviceJevReceipt, JevDevice, JevObservation, JevRun};

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

    #[test]
    fn task_handoff_context_upgrade_preserves_runs_and_detects_corrupt_text() {
        let root = test_root("task-handoff-context-upgrade");
        let request_digest = Sha256Digest(format!("sha256:{}", "a".repeat(64)));
        let store = AdapterStore::open(&root).unwrap();
        store
            .save_run("existing-run", &serde_json::json!({"existing":"state"}))
            .unwrap();
        store
            .lock()
            .unwrap()
            .execute_batch("DROP TABLE model_call_task_context")
            .unwrap();
        drop(store);
        let upgraded = AdapterStore::open(&root).unwrap();
        let run: serde_json::Value = upgraded.load_run("existing-run").unwrap().unwrap();
        assert_eq!(run["existing"], "state");
        upgraded
            .retain_model_task_context(
                "existing-run",
                "request-1",
                &request_digest,
                b"Task: fixture",
            )
            .unwrap();
        upgraded.lock().unwrap().execute(
            "UPDATE model_call_task_context SET context_text=?1 WHERE run_key=?2 AND model_call_id=?3",
            rusqlite::params![b"changed text".as_slice(), "existing-run", "request-1"],
        ).unwrap();
        assert_eq!(
            upgraded.retain_model_task_context(
                "existing-run",
                "request-1",
                &request_digest,
                b"Task: fixture"
            ),
            Err(AdapterStoreError::Corrupt)
        );
        drop(upgraded);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cancellation_closes_pending_jev_without_overwriting_measured_usage() {
        let root = test_root("cancel-pending-accounting");
        let store = AdapterStore::open(&root).unwrap();
        let began = Instant("2030-01-01T00:00:00Z".into());
        let cancelled = Instant("2030-01-01T00:00:10Z".into());
        store
            .record_performance_start(
                "run",
                PerformanceOperationKind::PrimaryModel,
                "call",
                &began,
            )
            .unwrap();
        let pending: DeviceJevReceipt = DeviceJevReceipt {
            operation_id: "jev:exchange:1".into(),
            input_digest: "a".repeat(64),
            run: None,
        };
        store
            .retain_jev_performance("run", &pending, &began)
            .unwrap();
        assert_eq!(
            store
                .delegated_performance_totals("run")
                .unwrap()
                .pending_model_calls,
            2
        );
        store
            .close_unmeasured_primary("run", "call", &cancelled)
            .unwrap();
        store
            .close_pending_jev_parent("run", "exchange", "call", &cancelled)
            .unwrap();
        let totals = store.delegated_performance_totals("run").unwrap();
        assert_eq!(totals.pending_model_calls, 0);
        assert!(!totals.usage_complete);
        assert!(!totals.cost_complete);
        assert_eq!(
            store.performance_total_runtime("run", &cancelled).unwrap(),
            10_000
        );
        let usage = store
            .retained_outcome_usage("run", 10_000)
            .unwrap()
            .unwrap();
        assert_eq!(usage.tokens, None);
        assert_eq!(usage.cost_microunits, None);
        assert_eq!(usage.known_tokens, 0);
        let known = PerformanceOperationCompletion {
            usage_known: true,
            input_tokens: 2,
            output_tokens: 1,
            actual_cost_microunits: Some(4),
            ..Default::default()
        };
        store
            .record_performance_completion(
                "run",
                PerformanceOperationKind::PrimaryModel,
                "call",
                &cancelled,
                known,
            )
            .unwrap();
        store
            .close_unmeasured_primary("run", "call", &cancelled)
            .unwrap();
        assert_eq!(
            store
                .execution_outcome_usage("run", 10_000)
                .unwrap()
                .known_tokens,
            3
        );
        drop(store);
        let store = AdapterStore::open(&root).unwrap();
        store
            .close_unmeasured_primary("run", "call", &cancelled)
            .unwrap();
        assert_eq!(
            store
                .execution_outcome_usage("run", 10_000)
                .unwrap()
                .known_tokens,
            3
        );
        assert_eq!(
            store
                .delegated_performance_totals("run")
                .unwrap()
                .pending_model_calls,
            0
        );
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn jev_receipts_settle_once_and_preserve_unknown_usage() {
        let root = test_root("jev-accounting");
        let store = AdapterStore::open(&root).unwrap();
        let now = Instant("2030-01-01T00:00:00Z".into());
        store
            .record_performance_completion(
                "run",
                PerformanceOperationKind::PrimaryModel,
                "primary",
                &now,
                PerformanceOperationCompletion {
                    usage_known: true,
                    input_tokens: 10,
                    ..Default::default()
                },
            )
            .unwrap();
        store
            .lock()
            .unwrap()
            .execute_batch("DROP TABLE performance_jev_receipt")
            .unwrap();
        drop(store);
        let store = AdapterStore::open(&root).unwrap();
        assert_eq!(
            store
                .execution_outcome_usage("run", 0)
                .unwrap()
                .known_tokens,
            10
        );

        let mut receipt: DeviceJevReceipt = DeviceJevReceipt {
            operation_id: "jev:exchange:1".into(),
            input_digest: "a".repeat(64),
            run: None,
        };
        store.retain_jev_performance("run", &receipt, &now).unwrap();
        let pending = store.delegated_performance_totals("run").unwrap();
        assert_eq!(pending.pending_model_calls, 1);
        assert!(!pending.cost_complete);
        assert_eq!(
            store.retained_outcome_usage("run", 30).unwrap(),
            Some(winwincode_execution_port::generated::ExecutionOutcomeUsage::unknown(30, 10))
        );
        receipt.run = Some(JevRun {
            value: None,
            failures: vec![],
            observation: Some(JevObservation {
                provider_id: "jev-provider".into(),
                model_id: "jev-latest".into(),
                resolved_model_id: Some("jev-actual".into()),
                latency: std::time::Duration::from_millis(20),
                input_tokens: 40,
                output_tokens: Some(8),
                batch_size: 4,
                device: JevDevice::Remote,
                confidence: 0.9,
            }),
        });
        store.retain_jev_performance("run", &receipt, &now).unwrap();
        store.retain_jev_performance("run", &receipt, &now).unwrap();
        let usage = store.execution_outcome_usage("run", 30).unwrap();
        assert_eq!(usage.known_tokens, 58);
        assert_eq!(usage.cost_microunits, None);
        assert_eq!(
            store.retained_outcome_usage("run", 30).unwrap(),
            Some(usage.clone())
        );
        let totals = store.delegated_performance_totals("run").unwrap();
        assert_eq!(totals.primary_model_calls, 1);
        drop(store);
        let store = AdapterStore::open(&root).unwrap();
        store.retain_jev_performance("run", &receipt, &now).unwrap();
        assert_eq!(
            store
                .execution_outcome_usage("run", 30)
                .unwrap()
                .known_tokens,
            58
        );
        assert_eq!(
            store.retained_outcome_usage("run", 30).unwrap(),
            Some(usage)
        );
        assert_eq!(store.retained_outcome_usage("other-run", 30).unwrap(), None);
        let mut changed = receipt.clone();
        changed.input_digest = "b".repeat(64);
        assert!(store.retain_jev_performance("run", &changed, &now).is_err());
        let mut partial = receipt.clone();
        partial.operation_id = "jev:exchange:2".into();
        partial
            .run
            .as_mut()
            .unwrap()
            .observation
            .as_mut()
            .unwrap()
            .output_tokens = None;
        store.retain_jev_performance("run", &partial, &now).unwrap();
        let total = store.delegated_performance_totals("run").unwrap();
        assert_eq!(total.total_tokens, 98);
        assert_eq!(total.pending_model_calls, 0);
        assert!(!total.usage_complete);
        assert!(!total.cost_complete);
        let other_usage = store.execution_outcome_usage("other-run", 0).unwrap();
        assert_eq!(other_usage.known_tokens, 0);
        let stored: String = store.lock().unwrap().query_row(
            "SELECT receipt_json FROM performance_jev_receipt WHERE run_key='run' AND operation_id=?1",
            [partial.operation_id], |row| row.get(0)).unwrap();
        let saved: DeviceJevReceipt = serde_json::from_str(&stored).unwrap();
        assert_eq!(saved.run.unwrap().observation.unwrap().output_tokens, None);
        assert_judge_receipt_usage_is_counted_once(&store, &now, receipt);
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    fn assert_judge_receipt_usage_is_counted_once(
        store: &AdapterStore,
        now: &Instant,
        receipt: DeviceJevReceipt,
    ) {
        let mut judge: DeviceJevReceipt<[f32; 3]> = DeviceJevReceipt {
            operation_id: "judge:exchange:0".into(),
            input_digest: "c".repeat(64),
            run: None,
        };
        store
            .retain_jev_performance("judge-run", &judge, now)
            .unwrap();
        assert_eq!(
            store
                .delegated_performance_totals("judge-run")
                .unwrap()
                .pending_model_calls,
            1
        );
        let run = receipt.run.expect("primary JEV run");
        judge.run = Some(JevRun {
            value: Some([0.1, 0.8, 0.1]),
            observation: run.observation,
            failures: run.failures,
        });
        store
            .retain_jev_performance("judge-run", &judge, now)
            .unwrap();
        store
            .retain_jev_performance("judge-run", &judge, now)
            .unwrap();
        assert_eq!(
            store
                .execution_outcome_usage("judge-run", 0)
                .unwrap()
                .known_tokens,
            48
        );
        let totals = store.delegated_performance_totals("judge-run").unwrap();
        assert_eq!(totals.pending_model_calls, 0);
        assert!(!totals.cost_complete);
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
                    usage_known: true,
                    cache_breakdown_known: true,
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
                    usage_known: true,
                    cache_breakdown_known: true,
                    duration_millis: Some(9_999),
                    input_tokens: 9_999,
                    cached_tokens: 9_999,
                    output_tokens: 9_999,
                    actual_cost_microunits: Some(9_999),
                },
            )
            .expect_err("changed duplicate completion conflicts");
        store
            .mark_model_call_provider_final_with_accounting("run", "model-call", true)
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
        let outcome = store
            .execution_outcome_usage("run", 5_000)
            .expect("settled usage");
        assert_eq!(outcome.known_tokens, 125);
        assert_eq!(outcome.cost_microunits, Some(37));
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
                .expect_err("changed Observer settlement replay conflicts");

            let totals = store
                .delegated_performance_totals("run")
                .expect("read delegated totals");
            assert_eq!(totals.observer_calls, 1, "{name}");
            assert_eq!(totals.pending_model_calls, 0, "{name}");
            assert_eq!(totals.total_tokens, 9, "{name}");
            assert_eq!(totals.total_cost_microunits, expected_cost, "{name}");
            assert_eq!(totals.cost_complete, cost_complete, "{name}");
            let outcome = store
                .execution_outcome_usage("run", 123)
                .expect("outcome usage");
            assert_eq!(outcome.known_tokens, 9);
            assert_eq!(outcome.cost_microunits, actual_cost_microunits);
            assert_eq!(outcome.runtime_millis, 123);
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
            assert_eq!(
                store.retained_outcome_usage("run", 123).unwrap(),
                Some(winwincode_execution_port::generated::ExecutionOutcomeUsage::unknown(123, 0)),
                "{name}"
            );
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
        assert_eq!(report.primary_model_input_tokens, Some(90));
        assert_eq!(report.primary_model_cached_tokens, Some(10));
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
    fn mcp_approval_migration_preserves_existing_pending_and_resolved_records() {
        let root = test_root("mcp-approval-migration");
        let store = AdapterStore::open(&root).expect("open store");
        let operation = StoredApprovalOperation {
            approval_id: "apr-existing".into(),
            run_key: "run".into(),
            kernel_session_id: "session".into(),
            operation_kind: StoredApprovalOperationKind::Exec,
            operation_id: "call".into(),
            turn_id: Some("turn".into()),
            request_digest: format!("sha256:{}", "1".repeat(64)),
            detail: None,
            resolution_digest: None,
            state: StoredApprovalOperationState::Pending,
        };
        store
            .retain_approval_operation(&operation)
            .expect("pending operation");
        let resolved = StoredApprovalOperation {
            approval_id: "apr-resolved".into(),
            resolution_digest: Some(format!("sha256:{}", "2".repeat(64))),
            state: StoredApprovalOperationState::Resolved,
            ..operation.clone()
        };
        store
            .retain_approval_operation(&resolved)
            .expect("resolved operation");
        {
            let connection = store.lock().expect("database");
            let schema: String = connection
                .query_row(
                    "SELECT sql FROM sqlite_master WHERE name='approval_operation'",
                    [],
                    |r| r.get(0),
                )
                .expect("schema");
            let old_schema = schema
                .replace("approval_operation", "approval_operation_old")
                .replace(", 'mcp'", "");
            connection
                .execute_batch(&old_schema)
                .expect("old constraint");
            connection
                .execute_batch(
                    "INSERT INTO approval_operation_old SELECT * FROM approval_operation;
                DROP TABLE approval_operation;
                ALTER TABLE approval_operation_old RENAME TO approval_operation;",
                )
                .expect("old schema with records");
        }
        drop(store);
        for _ in 0..2 {
            let reopened = AdapterStore::open(&root).expect("migrate or reopen");
            for expected in [&operation, &resolved] {
                assert_eq!(
                    reopened
                        .load_approval_operation(&expected.approval_id)
                        .expect("existing record"),
                    Some(expected.clone())
                );
            }
            reopened
                .retain_approval_operation(&StoredApprovalOperation {
                    approval_id: "apr-mcp".into(),
                    operation_kind: StoredApprovalOperationKind::Mcp,
                    operation_id: "[\"server\",42]".into(),
                    ..operation.clone()
                })
                .expect("MCP accepted by migrated constraint");
        }
        std::fs::remove_dir_all(root).expect("cleanup");
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
            detail: None,
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
            detail: None,
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

    #[test]
    fn opaque_input_choice_identities_survive_restart_exactly() {
        let root = test_root("input-choice-identities");
        let operation = StoredInputOperation {
            input_request_id: "inp_AAAAAAAAAAAAAAAAAAAAAAAAAA".to_owned(),
            run_key: "run".to_owned(),
            kernel_session_id: "kernel-session".to_owned(),
            question_id: "continue".to_owned(),
            turn_id: "turn".to_owned(),
            request_digest: format!("sha256:{}", "1".repeat(64)),
            choice_identities: vec![
                StoredInputChoiceIdentity {
                    public_label: "Continue".to_owned(),
                    public_occurrence: 0,
                    choice_id: "ich_AAAAAAAAAAAAAAAAAAAAAAAAAA".to_owned(),
                },
                StoredInputChoiceIdentity {
                    public_label: "Continue".to_owned(),
                    public_occurrence: 1,
                    choice_id: "ich_BBBBBBBBBBBBBBBBBBBBBBBBBB".to_owned(),
                },
            ],
            resolution_digest: None,
            state: StoredInputOperationState::Pending,
        };
        {
            let store = AdapterStore::open(&root).expect("open input store");
            assert_eq!(
                store
                    .retain_input_operation(&operation)
                    .expect("retain input choice identities"),
                operation
            );
        }
        {
            let store = AdapterStore::open(&root).expect("reopen input store");
            assert_eq!(
                store
                    .load_input_operation(&operation.input_request_id)
                    .expect("load input choice identities after restart"),
                Some(operation)
            );
        }
        std::fs::remove_dir_all(root).expect("remove input store fixture");
    }

    #[test]
    fn input_choice_identity_migration_fails_legacy_rows_to_an_empty_mapping() {
        let root = test_root("input-choice-identity-migration");
        std::fs::create_dir_all(&root).expect("create legacy store root");
        {
            let connection = rusqlite::Connection::open(root.join(DATABASE_FILE))
                .expect("open legacy input store");
            connection
                .execute_batch(
                    "CREATE TABLE input_operation (
                       input_request_id TEXT PRIMARY KEY NOT NULL,
                       run_key TEXT NOT NULL,
                       kernel_session_id TEXT NOT NULL,
                       question_id TEXT NOT NULL,
                       turn_id TEXT NOT NULL,
                       request_digest TEXT NOT NULL,
                       resolution_digest TEXT,
                       state TEXT NOT NULL
                     );",
                )
                .expect("create legacy input operation table");
            connection
                .execute(
                    "INSERT INTO input_operation(
                       input_request_id, run_key, kernel_session_id, question_id, turn_id,
                       request_digest, resolution_digest, state
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, 'pending')",
                    rusqlite::params![
                        "inp_AAAAAAAAAAAAAAAAAAAAAAAAAA",
                        "run",
                        "kernel-session",
                        "continue",
                        "turn",
                        format!("sha256:{}", "1".repeat(64)),
                    ],
                )
                .expect("retain legacy input operation");
        }
        let store = AdapterStore::open(&root).expect("migrate legacy input store");
        let migrated = store
            .load_input_operation("inp_AAAAAAAAAAAAAAAAAAAAAAAAAA")
            .expect("load migrated input operation")
            .expect("migrated input operation exists");
        assert!(migrated.choice_identities.is_empty());
        std::fs::remove_dir_all(root).expect("remove migration fixture");
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
    #[test]
    fn independent_usage_reconciliation_preserves_original_terminal_and_old_fact_replay() {
        let root = test_root("independent-usage-replay");
        let store = AdapterStore::open(&root).expect("store");
        let started = Instant("2030-01-01T00:00:00.000Z".into());
        let terminal = Instant("2030-01-01T00:00:01.000Z".into());
        let token_at = Instant("2030-01-01T00:00:02.000Z".into());
        let cost_at = Instant("2030-01-01T00:00:03.000Z".into());
        let unknown = PerformanceOperationCompletion {
            usage_known: false,
            duration_millis: Some(1000),
            ..Default::default()
        };
        store
            .record_performance_start(
                "run",
                PerformanceOperationKind::PrimaryModel,
                "call",
                &started,
            )
            .expect("start");
        store
            .record_performance_completion(
                "run",
                PerformanceOperationKind::PrimaryModel,
                "call",
                &terminal,
                unknown,
            )
            .expect("terminal");
        let tokens = PerformanceOperationCompletion {
            usage_known: true,
            input_tokens: 80,
            output_tokens: 20,
            duration_millis: Some(1000),
            ..Default::default()
        };
        store
            .settle_performance_usage(
                "run",
                PerformanceOperationKind::PrimaryModel,
                "call",
                &token_at,
                tokens,
            )
            .expect("tokens");
        let cost = PerformanceOperationCompletion {
            actual_cost_microunits: Some(7),
            ..tokens
        };
        store
            .settle_performance_usage(
                "run",
                PerformanceOperationKind::PrimaryModel,
                "call",
                &cost_at,
                cost,
            )
            .expect("cost");
        drop(store);
        let store = AdapterStore::open(&root).expect("reopen");
        store
            .record_performance_completion(
                "run",
                PerformanceOperationKind::PrimaryModel,
                "call",
                &terminal,
                unknown,
            )
            .expect("original terminal replay");
        store
            .settle_performance_usage(
                "run",
                PerformanceOperationKind::PrimaryModel,
                "call",
                &token_at,
                tokens,
            )
            .expect("older fact replay");
        let totals = store.delegated_performance_totals("run").expect("totals");
        assert_eq!(totals.total_tokens, 100);
        assert_eq!(totals.total_cost_microunits, 7);
        assert!(totals.usage_complete && totals.cost_complete);
        assert_eq!(totals.pending_model_calls, 0);
        drop(store);
        std::fs::remove_dir_all(root).expect("cleanup");
    }
    #[test]
    fn failed_legacy_statistics_backfill_keeps_jev_usage_unknown_after_restart() {
        let root = test_root("legacy-jev-stats-backfill");
        let store = AdapterStore::open(&root).unwrap();
        let receipt = serde_json::json!({"operationId":"jev:exchange:0", "inputDigest":"old",
            "run":{"observation":{"inputTokens":5,"outputTokens":null},"failures":[]}})
        .to_string();
        store.lock().unwrap().execute(
            "INSERT INTO performance_jev_receipt(run_key,operation_id,receipt_json,completed,input_tokens,output_tokens,usage_known)
             VALUES('run','jev:exchange:0',?1,1,5,0,1)", [receipt]).unwrap();
        store
            .lock()
            .unwrap()
            .execute_batch(
                "CREATE TRIGGER deny_optional_jev_backfill BEFORE UPDATE ON performance_jev_receipt
             BEGIN SELECT RAISE(FAIL,'optional backfill unavailable'); END;",
            )
            .unwrap();
        drop(store);
        let restored = AdapterStore::open(&root)
            .expect("auxiliary backfill failure does not block execution recovery");
        let totals = restored.delegated_performance_totals("run").unwrap();
        assert!(
            !totals.usage_complete,
            "an old optimistic flag cannot make missing tokens known"
        );
        let usage = restored.execution_outcome_usage("run", 0).unwrap();
        assert_eq!(usage.tokens, None);
        assert_eq!(usage.known_tokens, 5);
        drop(restored);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unknown_cache_split_keeps_total_tokens_and_can_be_settled_after_restart() {
        let root = test_root("cache-split-restart");
        let store = AdapterStore::open(&root).unwrap();
        store
            .register_performance_run("run", ExecutionMode::React, ObserverMode::Off)
            .unwrap();
        let at = Instant("2030-01-01T00:00:01.000Z".into());
        let usage = PerformanceOperationCompletion {
            usage_known: true,
            cache_breakdown_known: false,
            input_tokens: 100,
            cached_tokens: 0,
            output_tokens: 20,
            actual_cost_microunits: None,
            ..Default::default()
        };
        store
            .record_performance_completion(
                "run",
                PerformanceOperationKind::PrimaryModel,
                "call",
                &at,
                usage,
            )
            .unwrap();
        let report = store.performance_report("run", 1000).unwrap();
        assert_eq!(report.primary_model_input_tokens, None);
        assert_eq!(report.primary_model_cached_tokens, None);
        assert_eq!(report.primary_model_inclusive_input_tokens, Some(100));
        assert!(report.usage_complete);
        let totals = store.delegated_performance_totals("run").unwrap();
        assert_eq!(totals.total_tokens, 120);
        assert!(!totals.cost_complete);
        drop(store);
        let store = AdapterStore::open(&root).unwrap();
        let known = PerformanceOperationCompletion {
            cache_breakdown_known: true,
            input_tokens: 80,
            cached_tokens: 20,
            ..usage
        };
        let later = Instant("2030-01-01T00:00:02.000Z".into());
        store
            .settle_performance_usage(
                "run",
                PerformanceOperationKind::PrimaryModel,
                "call",
                &later,
                known,
            )
            .unwrap();
        store
            .settle_performance_usage(
                "run",
                PerformanceOperationKind::PrimaryModel,
                "call",
                &later,
                known,
            )
            .unwrap();
        let report = store.performance_report("run", 1000).unwrap();
        assert_eq!(report.primary_model_input_tokens, Some(80));
        assert_eq!(report.primary_model_cached_tokens, Some(20));
        assert_eq!(
            store
                .delegated_performance_totals("run")
                .unwrap()
                .total_tokens,
            120
        );
        let changed = PerformanceOperationCompletion {
            input_tokens: 79,
            ..known
        };
        assert_eq!(
            store.settle_performance_usage(
                "run",
                PerformanceOperationKind::PrimaryModel,
                "call",
                &Instant("2030-01-01T00:00:03.000Z".into()),
                changed
            ),
            Err(AdapterStoreError::Conflict)
        );
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[cfg(test)]
#[path = "storage_indexed_regression.rs"]
mod indexed_regression_tests;
