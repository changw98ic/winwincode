// SPDX-License-Identifier: Apache-2.0

use std::{
    fs,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use rusqlite::{Connection, OpenFlags, types::Value};
use sha2::{Digest as _, Sha256};

use crate::{DeviceProviderError, device_execution_source::ProviderExecutionSource};

const DATABASE: &str = "provider-runtime.sqlite3";

pub struct ProviderRuntimeStore {
    pub(crate) connection: Connection,
    pub(crate) execution_source: Option<Arc<dyn ProviderExecutionSource>>,
    directory: PathBuf,
}

struct RuntimeTable {
    name: &'static str,
    columns: &'static str,
    all_columns: &'static str,
    order: &'static str,
}

impl RuntimeTable {
    fn has_original_receipt_digest(&self) -> bool {
        matches!(self.name, "jev_context_exchanges" | "jev_judge_exchanges")
    }

    fn runtime_columns(&self, columns: &str) -> String {
        if self.has_original_receipt_digest() {
            format!("{columns},original_receipt_digest")
        } else {
            columns.to_owned()
        }
    }
}

const TABLES: &[RuntimeTable] = &[
    RuntimeTable {
        name: "exchanges",
        columns: "exchange_id,digest,chunks,cancelled,request_open,prepared_payload,prepared_payload_digest,accounting_chunks",
        all_columns: "exchange_id,digest,chunks,cancelled,request_open,prepared_payload,prepared_payload_digest,accounting_chunks,call_status",
        order: "exchange_id",
    },
    RuntimeTable {
        name: "jev_context_exchanges",
        columns: "operation_id,digest,request_json,result,call_status",
        all_columns: "operation_id,digest,request_json,result,call_status",
        order: "operation_id",
    },
    RuntimeTable {
        name: "jev_judge_exchanges",
        columns: "operation_id,request_json,result,call_status",
        all_columns: "operation_id,request_json,result,call_status",
        order: "operation_id",
    },
    RuntimeTable {
        name: "accounting_closed_attempts",
        columns: "job_id,attempt,lease_id",
        all_columns: "job_id,attempt,lease_id",
        order: "job_id,attempt",
    },
    RuntimeTable {
        name: "model_invocation_attempts",
        columns: "exchange_id,attempt_number,adapter_request_id,state,request_json,request_payload,failure_chunks,accounting_chunks,response_bytes,response_bytes_received",
        all_columns: "exchange_id,attempt_number,adapter_request_id,state,request_json,request_payload,failure_chunks,accounting_chunks,response_bytes,response_bytes_received,call_status",
        order: "exchange_id,attempt_number",
    },
    RuntimeTable {
        name: "model_attempt_diagnostics",
        columns: "exchange_id,sequence,policy_attempt,started_ms,finished_ms,outcome,failure_json,stop_reason",
        all_columns: "exchange_id,sequence,policy_attempt,started_ms,finished_ms,outcome,failure_json,stop_reason",
        order: "exchange_id,sequence",
    },
    RuntimeTable {
        name: "jev_attempt_diagnostics",
        columns: "operation_id,sequence,role,recorded_ms,failure_json",
        all_columns: "operation_id,sequence,role,recorded_ms,failure_json",
        order: "operation_id,sequence",
    },
    RuntimeTable {
        name: "model_retry_state",
        columns: "exchange_id,policy_attempt,connection_attempt,wake_ms,decision",
        all_columns: "exchange_id,policy_attempt,connection_attempt,wake_ms,decision",
        order: "exchange_id",
    },
    RuntimeTable {
        name: "jev_invocation_attempts",
        columns: "operation_id,sequence,request_json,status,result_json",
        all_columns: "operation_id,sequence,request_json,status,result_json",
        order: "operation_id,sequence",
    },
    RuntimeTable {
        name: "model_response_frame",
        columns: "exchange_id,sequence,frame_json,frame_digest",
        all_columns: "exchange_id,sequence,frame_json,frame_digest",
        order: "exchange_id,sequence",
    },
    RuntimeTable {
        name: "model_response_exchange",
        columns: "exchange_id,worker_session_id,worker_instance_id,highest_sequence,response_digest,binding_json",
        all_columns: "exchange_id,worker_session_id,worker_instance_id,highest_sequence,response_digest,binding_json",
        order: "exchange_id",
    },
];

impl ProviderRuntimeStore {
    pub fn open_existing(directory: &Path) -> Result<Self, DeviceProviderError> {
        checked_directory(directory)?;
        let directory = fs::canonicalize(directory)?;
        let path = directory.join(DATABASE);
        checked_file(&path)?;
        let connection = Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        connection.busy_timeout(Duration::from_secs(5))?;
        validate_runtime(&connection)?;
        configure(&connection)?;
        Ok(Self {
            connection,
            execution_source: None,
            directory,
        })
    }

    #[must_use]
    pub fn with_execution_source(mut self, source: Arc<dyn ProviderExecutionSource>) -> Self {
        self.execution_source = Some(source);
        self
    }

    pub(crate) fn execution_source(
        &self,
    ) -> Result<&dyn ProviderExecutionSource, DeviceProviderError> {
        self.execution_source
            .as_deref()
            .ok_or(DeviceProviderError::StateInvalid)
    }

    #[must_use]
    pub fn directory(&self) -> &Path {
        &self.directory
    }

    pub(crate) fn sse_failure_log_directory(&self) -> Result<PathBuf, DeviceProviderError> {
        Ok(Path::new(
            self.connection
                .path()
                .ok_or(DeviceProviderError::StateInvalid)?,
        )
        .parent()
        .ok_or(DeviceProviderError::StateInvalid)?
        .join("logs")
        .join("sse-failures"))
    }

    pub(crate) fn source_id(&self) -> Result<String, DeviceProviderError> {
        Ok(self.connection.query_row(
            "SELECT source_id FROM runtime_origin WHERE singleton=1",
            [],
            |row| row.get(0),
        )?)
    }
}

pub(crate) fn create_runtime_schema(connection: &Connection) -> Result<(), DeviceProviderError> {
    connection.execute_batch(
        "CREATE TABLE exchanges (
            exchange_id TEXT PRIMARY KEY,
            digest TEXT NOT NULL,
            chunks TEXT,
            cancelled INTEGER NOT NULL DEFAULT 0,
            request_open TEXT,
            prepared_payload BLOB,
            prepared_payload_digest TEXT,
            accounting_chunks TEXT,
            call_status TEXT GENERATED ALWAYS AS (CASE WHEN request_open IS NULL THEN NULL WHEN chunks IS NULL THEN 'pending' WHEN NOT json_valid(chunks) THEN 'failed' WHEN json_extract(chunks,'$[#-1].error.message')='DEVICE_MODEL_CANCELLED' THEN 'cancelled' WHEN json_extract(chunks,'$[#-1].error') IS NOT NULL THEN 'failed' ELSE 'succeeded' END) VIRTUAL);
         CREATE TABLE jev_context_exchanges (
            operation_id TEXT PRIMARY KEY,
            digest TEXT NOT NULL,
            request_json TEXT NOT NULL,
            result TEXT,
            call_status TEXT NOT NULL DEFAULT 'pending' CHECK(call_status IN ('pending','succeeded','failed','cancelled')),
            original_receipt_digest TEXT,
            CHECK((call_status='pending' AND result IS NULL) OR (call_status!='pending' AND result IS NOT NULL)));
         CREATE TABLE jev_judge_exchanges (
            operation_id TEXT PRIMARY KEY,
            request_json TEXT NOT NULL,
            result TEXT,
            call_status TEXT NOT NULL DEFAULT 'pending' CHECK(call_status IN ('pending','succeeded','failed','cancelled')),
            original_receipt_digest TEXT,
            CHECK((call_status='pending' AND result IS NULL) OR (call_status!='pending' AND result IS NOT NULL)));
         CREATE TABLE accounting_closed_attempts (
            job_id TEXT NOT NULL,
            attempt INTEGER NOT NULL,
            lease_id TEXT NOT NULL,
            PRIMARY KEY(job_id,attempt));
         CREATE TABLE model_invocation_attempts (
            exchange_id TEXT NOT NULL,
            attempt_number INTEGER NOT NULL CHECK(attempt_number>0),
            adapter_request_id TEXT NOT NULL UNIQUE,
            state TEXT NOT NULL,
            request_json TEXT NOT NULL,
            request_payload BLOB NOT NULL,
            failure_chunks TEXT,
            accounting_chunks TEXT,
            response_bytes BLOB,
            response_bytes_received INTEGER,
            call_status TEXT GENERATED ALWAYS AS (CASE WHEN state IN ('prepared','invoking') THEN 'pending' WHEN state='completed' THEN 'succeeded' WHEN json_valid(failure_chunks) AND json_extract(failure_chunks,'$[#-1].error.message')='DEVICE_MODEL_CANCELLED' THEN 'cancelled' ELSE 'failed' END) VIRTUAL,
            PRIMARY KEY(exchange_id,attempt_number),
            CHECK(state IN ('prepared','invoking') OR failure_chunks IS NOT NULL OR accounting_chunks IS NOT NULL));
         CREATE TABLE model_attempt_diagnostics (
            exchange_id TEXT NOT NULL REFERENCES exchanges(exchange_id),
            sequence INTEGER NOT NULL CHECK(sequence>0),
            policy_attempt INTEGER NOT NULL CHECK(policy_attempt BETWEEN 1 AND 4),
            started_ms INTEGER NOT NULL,
            finished_ms INTEGER,
            outcome TEXT NOT NULL CHECK(outcome IN ('in_flight','accepted','failed')),
            failure_json TEXT,
            stop_reason TEXT,
            PRIMARY KEY(exchange_id,sequence));
         CREATE TABLE jev_attempt_diagnostics (
            operation_id TEXT NOT NULL,
            sequence INTEGER NOT NULL CHECK(sequence>0),
            role TEXT NOT NULL CHECK(role IN ('context','judge')),
            recorded_ms INTEGER NOT NULL,
            failure_json TEXT NOT NULL,
            PRIMARY KEY(operation_id,sequence));
         CREATE TABLE model_retry_state (
            exchange_id TEXT PRIMARY KEY REFERENCES exchanges(exchange_id),
            policy_attempt INTEGER NOT NULL CHECK(policy_attempt BETWEEN 1 AND 4),
            connection_attempt INTEGER NOT NULL DEFAULT 0 CHECK(connection_attempt>=0),
            wake_ms INTEGER NOT NULL,
            decision TEXT NOT NULL CHECK(decision IN ('retry_after','deferred_until')));
         CREATE TABLE jev_invocation_attempts (
            operation_id TEXT NOT NULL,
            sequence INTEGER NOT NULL,
            request_json TEXT NOT NULL,
            status TEXT NOT NULL DEFAULT 'pending' CHECK(status IN ('pending','succeeded','failed','cancelled')),
            result_json TEXT,
            PRIMARY KEY(operation_id,sequence),
            CHECK((status='pending' AND result_json IS NULL) OR (status!='pending' AND result_json IS NOT NULL)));",
    )?;
    crate::model_response_store::create_model_response_index(connection)
}

pub(crate) fn migrate_runtime(
    source: &Connection,
    directory: &Path,
    source_id: &str,
) -> Result<ProviderRuntimeStore, DeviceProviderError> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(directory)?;
    checked_directory(directory)?;
    let target = directory.join(DATABASE);
    if target.try_exists()? {
        let runtime = ProviderRuntimeStore::open_existing(directory)?;
        validate_runtime_schema(&runtime.connection)?;
        if runtime.source_id()? != source_id {
            return Err(DeviceProviderError::StateInvalid);
        }
        verify_copy(source, &runtime.connection)?;
        return Ok(runtime);
    }
    let staging = directory.join(format!(".provider-runtime-{source_id}.sqlite3"));
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&staging)
    {
        Ok(file) => file.sync_all()?,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.into()),
    }
    checked_file(&staging)?;
    let mut destination = Connection::open_with_flags(
        &staging,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    destination.busy_timeout(Duration::from_secs(5))?;
    destination.execute_batch(
        "PRAGMA journal_mode=DELETE; PRAGMA synchronous=FULL; PRAGMA secure_delete=ON;",
    )?;
    let version: i64 = destination.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version == 0 {
        let transaction =
            destination.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let objects: i64 = transaction.query_row(
            "SELECT count(*) FROM sqlite_master WHERE name NOT LIKE 'sqlite_%'",
            [],
            |row| row.get(0),
        )?;
        if objects != 0 {
            return Err(DeviceProviderError::StateInvalid);
        }
        create_runtime_schema(&transaction)?;
        copy_rows(source, &transaction)?;
        verify_copy(source, &transaction)?;
        transaction.execute_batch("CREATE TABLE runtime_origin(singleton INTEGER PRIMARY KEY CHECK(singleton=1),source_id TEXT NOT NULL); PRAGMA user_version=1;")?;
        transaction.execute("INSERT INTO runtime_origin VALUES(1,?1)", [source_id])?;
        transaction.commit()?;
    } else {
        validate_runtime_schema(&destination)?;
        let origin: String = destination.query_row(
            "SELECT source_id FROM runtime_origin WHERE singleton=1",
            [],
            |row| row.get(0),
        )?;
        if origin != source_id {
            return Err(DeviceProviderError::StateInvalid);
        }
        verify_copy(source, &destination)?;
    }
    validate_runtime_schema(&destination)?;
    destination
        .close()
        .map_err(|(_, error)| DeviceProviderError::from(error))?;
    fs::File::open(&staging)?.sync_all()?;
    fs::hard_link(&staging, &target)?;
    fs::File::open(directory)?.sync_all()?;
    fs::remove_file(staging)?;
    fs::File::open(directory)?.sync_all()?;
    ProviderRuntimeStore::open_existing(directory)
}

fn copy_rows(source: &Connection, destination: &Connection) -> Result<(), DeviceProviderError> {
    for table in TABLES {
        let selected = table
            .columns
            .replace("response_bytes,", "NULL AS response_bytes,");
        let mut query = source.prepare(&format!(
            "SELECT {selected} FROM {} ORDER BY {}",
            table.name, table.order
        ))?;
        let mut rows = query.query([])?;
        let columns = table.runtime_columns(table.columns);
        let placeholders = vec!["?"; columns.split(',').count()].join(",");
        let mut insert = destination.prepare(&format!(
            "INSERT INTO {}({}) VALUES({placeholders})",
            table.name, columns
        ))?;
        while let Some(row) = rows.next()? {
            let values =
                project_legacy_row(table, read_values(row, table.columns.split(',').count())?)?;
            insert.execute(rusqlite::params_from_iter(values.iter()))?;
        }
    }
    Ok(())
}

fn verify_copy(source: &Connection, destination: &Connection) -> Result<(), DeviceProviderError> {
    for table in TABLES {
        let selected = table
            .all_columns
            .replace("response_bytes,", "NULL AS response_bytes,");
        let mut old_query = source.prepare(&format!(
            "SELECT {selected} FROM {} ORDER BY {}",
            table.name, table.order
        ))?;
        let columns = table.runtime_columns(table.all_columns);
        let mut new_query = destination.prepare(&format!(
            "SELECT {} FROM {} ORDER BY {}",
            columns, table.name, table.order
        ))?;
        let mut old_rows = old_query.query([])?;
        let mut new_rows = new_query.query([])?;
        loop {
            match (old_rows.next()?, new_rows.next()?) {
                (None, None) => break,
                (Some(old), Some(new))
                    if project_legacy_row(
                        table,
                        read_values(old, table.all_columns.split(',').count())?,
                    )? == read_values(new, columns.split(',').count())? => {}
                _ => return Err(DeviceProviderError::StateInvalid),
            }
        }
    }
    Ok(())
}

fn project_legacy_row(
    table: &RuntimeTable,
    mut values: Vec<Value>,
) -> Result<Vec<Value>, DeviceProviderError> {
    let result_index = match table.name {
        "jev_context_exchanges" => 3,
        "jev_judge_exchanges" => 2,
        "jev_invocation_attempts" => 4,
        _ => return Ok(values),
    };
    let mut original_digest = Value::Null;
    if let Value::Text(original) = &values[result_index]
        && let Some(redacted) =
            redacted_jev_result(original, table.name == "jev_context_exchanges")?
    {
        original_digest = match table.name {
            "jev_context_exchanges" => {
                Value::Text(original_jev_receipt_digest::<crate::JevContextEvaluation>(
                    stored_text(&values[0])?,
                    stored_text(&values[1])?.to_owned(),
                    original,
                )?)
            }
            "jev_judge_exchanges" => Value::Text(original_jev_receipt_digest::<[f32; 3]>(
                stored_text(&values[0])?,
                format!("{:x}", Sha256::digest(stored_text(&values[1])?.as_bytes())),
                original,
            )?),
            _ => Value::Null,
        };
        values[result_index] = Value::Text(redacted);
    }
    if table.has_original_receipt_digest() {
        values.push(original_digest);
    }
    Ok(values)
}

fn stored_text(value: &Value) -> Result<&str, DeviceProviderError> {
    if let Value::Text(text) = value {
        Ok(text)
    } else {
        Err(DeviceProviderError::StateInvalid)
    }
}

fn original_jev_receipt_digest<T: serde::Serialize + serde::de::DeserializeOwned>(
    operation_id: &str,
    input_digest: String,
    result: &str,
) -> Result<String, DeviceProviderError> {
    let receipt = crate::DeviceJevReceipt {
        operation_id: operation_id.to_owned(),
        input_digest,
        run: Some(serde_json::from_str::<crate::JevRun<T>>(result)?),
    };
    Ok(format!(
        "sha256:{:x}",
        Sha256::digest(serde_json::to_vec(&receipt)?)
    ))
}

fn redacted_jev_result(
    original: &str,
    context: bool,
) -> Result<Option<String>, DeviceProviderError> {
    let Ok(mut result) = serde_json::from_str::<serde_json::Value>(original) else {
        return Ok(None);
    };
    let mut changed = false;
    for pointer in ["/value/resolved_model_id", "/observation/resolvedModelId"] {
        if let Some(value) = result.pointer_mut(pointer)
            && !value.is_null()
        {
            *value = serde_json::Value::Null;
            changed = true;
        }
    }
    if context
        && let Some(requested) = result.pointer("/observation/modelId").cloned()
        && let Some(model) = result.pointer_mut("/value/decision/model")
        && *model != requested
    {
        *model = requested;
        changed = true;
    }
    changed
        .then(|| serde_json::to_string(&result))
        .transpose()
        .map_err(DeviceProviderError::from)
}

fn read_values(row: &rusqlite::Row<'_>, count: usize) -> Result<Vec<Value>, rusqlite::Error> {
    (0..count).map(|index| row.get(index)).collect()
}

fn validate_runtime(connection: &Connection) -> Result<(), DeviceProviderError> {
    let version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version != 1 {
        return Err(DeviceProviderError::StateInvalid);
    }
    let source_id: String = connection.query_row(
        "SELECT source_id FROM runtime_origin WHERE singleton=1",
        [],
        |row| row.get(0),
    )?;
    if source_id.len() != 64 || !source_id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(DeviceProviderError::StateInvalid);
    }
    Ok(())
}

fn validate_runtime_schema(connection: &Connection) -> Result<(), DeviceProviderError> {
    validate_runtime(connection)?;
    let mut expected = TABLES
        .iter()
        .map(|table| table.name.to_owned())
        .chain(std::iter::once("runtime_origin".to_owned()))
        .collect::<Vec<_>>();
    expected.sort();
    let actual=connection.prepare("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name")?.query_map([],|row|row.get::<_,String>(0))?.collect::<Result<Vec<_>,_>>()?;
    if actual != expected {
        return Err(DeviceProviderError::StateInvalid);
    }
    for table in TABLES {
        let names = connection
            .prepare(&format!("PRAGMA table_xinfo({})", table.name))?
            .query_map([], |row| row.get::<_, String>(1))?
            .collect::<Result<Vec<_>, _>>()?;
        if names.join(",") != table.runtime_columns(table.all_columns) {
            return Err(DeviceProviderError::StateInvalid);
        }
    }
    Ok(())
}

fn configure(connection: &Connection) -> Result<(), DeviceProviderError> {
    crate::device_store::enable_device_wal(connection)?;
    connection.execute_batch("PRAGMA synchronous=FULL; PRAGMA secure_delete=ON;")?;
    Ok(())
}

fn checked_directory(path: &Path) -> Result<(), DeviceProviderError> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir() || metadata.permissions().mode() & 0o077 != 0 {
        return Err(DeviceProviderError::StateInvalid);
    }
    Ok(())
}

fn checked_file(path: &Path) -> Result<(), DeviceProviderError> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.permissions().mode() & 0o077 != 0 {
        return Err(DeviceProviderError::StateInvalid);
    }
    Ok(())
}

#[cfg(test)]
#[path = "device_runtime_store_tests.rs"]
mod tests;
