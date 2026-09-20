// SPDX-License-Identifier: Apache-2.0

//! Durable Server facts for the Device-owned managed application lifecycle.

use rusqlite::{Connection, OptionalExtension, params};

use crate::{StorageError, sql_error};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManagedAppRunConfigRecord {
    pub run_id: String,
    pub work_run_id: String,
    pub repository_binding_id: String,
    pub attempt: i64,
    pub config_json: Vec<u8>,
}

/// Server-owned repository template used to materialize one `WorkRun` config.
/// It contains no run, candidate, or source identity.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManagedAppRunTemplateRecord {
    pub repository_binding_id: String,
    pub revision: i64,
    pub template_json: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManagedAppStatusRecord {
    pub client_node_id: String,
    pub run_id: String,
    pub status_json: Vec<u8>,
    pub occurred_at: String,
}

pub(crate) fn create_schema(connection: &rusqlite::Transaction<'_>) -> Result<(), StorageError> {
    connection
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS managed_app_run_configs (
                 run_id TEXT PRIMARY KEY NOT NULL,
                 work_run_id TEXT NOT NULL UNIQUE,
                 repository_binding_id TEXT NOT NULL,
                 attempt INTEGER NOT NULL CHECK (attempt > 0),
                 config_json BLOB NOT NULL
             );
             CREATE TABLE IF NOT EXISTS managed_app_run_templates (
                 repository_binding_id TEXT PRIMARY KEY NOT NULL,
                 revision INTEGER NOT NULL CHECK (revision > 0),
                 template_json BLOB NOT NULL
             );
             CREATE TABLE IF NOT EXISTS managed_app_statuses (
                 client_node_id TEXT NOT NULL,
                 run_id TEXT NOT NULL,
                 status_json BLOB NOT NULL,
                 occurred_at TEXT NOT NULL,
                 PRIMARY KEY (client_node_id, run_id)
             );",
        )
        .map_err(sql_error)
}

pub(crate) fn save_run_template(
    connection: &Connection,
    record: &ManagedAppRunTemplateRecord,
) -> Result<(), StorageError> {
    if record.repository_binding_id.is_empty()
        || record.revision <= 0
        || record.template_json.is_empty()
    {
        return Err(StorageError::invalid(
            "managed application template is invalid",
        ));
    }
    connection
        .execute(
            "INSERT INTO managed_app_run_templates
             (repository_binding_id, revision, template_json)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(repository_binding_id) DO UPDATE SET
               revision = excluded.revision,
               template_json = excluded.template_json
             WHERE excluded.revision >= managed_app_run_templates.revision",
            params![
                record.repository_binding_id,
                record.revision,
                record.template_json,
            ],
        )
        .map_err(sql_error)?;
    Ok(())
}

pub(crate) fn load_run_template(
    connection: &Connection,
    repository_binding_id: &str,
) -> Result<Option<ManagedAppRunTemplateRecord>, StorageError> {
    connection
        .query_row(
            "SELECT repository_binding_id, revision, template_json
             FROM managed_app_run_templates WHERE repository_binding_id = ?1",
            [repository_binding_id],
            |row| {
                Ok(ManagedAppRunTemplateRecord {
                    repository_binding_id: row.get(0)?,
                    revision: row.get(1)?,
                    template_json: row.get(2)?,
                })
            },
        )
        .optional()
        .map_err(sql_error)
}

pub(crate) fn save_run_config(
    connection: &Connection,
    record: &ManagedAppRunConfigRecord,
) -> Result<(), StorageError> {
    if record.run_id.is_empty()
        || record.work_run_id.is_empty()
        || record.repository_binding_id.is_empty()
        || record.attempt <= 0
        || record.config_json.is_empty()
    {
        return Err(StorageError::invalid(
            "managed application config is invalid",
        ));
    }
    connection
        .execute(
            "INSERT INTO managed_app_run_configs
             (run_id, work_run_id, repository_binding_id, attempt, config_json)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(run_id) DO UPDATE SET
               work_run_id = excluded.work_run_id,
               repository_binding_id = excluded.repository_binding_id,
               attempt = excluded.attempt,
               config_json = excluded.config_json
             WHERE managed_app_run_configs.work_run_id = excluded.work_run_id",
            params![
                record.run_id,
                record.work_run_id,
                record.repository_binding_id,
                record.attempt,
                record.config_json,
            ],
        )
        .map_err(sql_error)?;
    Ok(())
}

pub(crate) fn load_run_config(
    connection: &Connection,
    run_id: &str,
) -> Result<Option<ManagedAppRunConfigRecord>, StorageError> {
    connection
        .query_row(
            "SELECT run_id, work_run_id, repository_binding_id, attempt, config_json
             FROM managed_app_run_configs WHERE run_id = ?1",
            [run_id],
            |row| {
                Ok(ManagedAppRunConfigRecord {
                    run_id: row.get(0)?,
                    work_run_id: row.get(1)?,
                    repository_binding_id: row.get(2)?,
                    attempt: row.get(3)?,
                    config_json: row.get(4)?,
                })
            },
        )
        .optional()
        .map_err(sql_error)
}

pub(crate) fn load_run_config_for_work_run(
    connection: &Connection,
    work_run_id: &str,
) -> Result<Option<ManagedAppRunConfigRecord>, StorageError> {
    connection
        .query_row(
            "SELECT run_id, work_run_id, repository_binding_id, attempt, config_json
             FROM managed_app_run_configs WHERE work_run_id = ?1",
            [work_run_id],
            |row| {
                Ok(ManagedAppRunConfigRecord {
                    run_id: row.get(0)?,
                    work_run_id: row.get(1)?,
                    repository_binding_id: row.get(2)?,
                    attempt: row.get(3)?,
                    config_json: row.get(4)?,
                })
            },
        )
        .optional()
        .map_err(sql_error)
}

pub(crate) fn save_status(
    connection: &Connection,
    record: &ManagedAppStatusRecord,
) -> Result<(), StorageError> {
    if record.client_node_id.is_empty() || record.run_id.is_empty() || record.status_json.is_empty()
    {
        return Err(StorageError::invalid(
            "managed application status is invalid",
        ));
    }
    connection
        .execute(
            "INSERT INTO managed_app_statuses
             (client_node_id, run_id, status_json, occurred_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(client_node_id, run_id) DO UPDATE SET
               status_json = excluded.status_json,
               occurred_at = excluded.occurred_at
             WHERE excluded.occurred_at >= managed_app_statuses.occurred_at",
            params![
                record.client_node_id,
                record.run_id,
                record.status_json,
                record.occurred_at,
            ],
        )
        .map_err(sql_error)?;
    Ok(())
}

pub(crate) fn load_status(
    connection: &Connection,
    client_node_id: &str,
    run_id: &str,
) -> Result<Option<ManagedAppStatusRecord>, StorageError> {
    connection
        .query_row(
            "SELECT client_node_id, run_id, status_json, occurred_at
             FROM managed_app_statuses WHERE client_node_id = ?1 AND run_id = ?2",
            params![client_node_id, run_id],
            |row| {
                Ok(ManagedAppStatusRecord {
                    client_node_id: row.get(0)?,
                    run_id: row.get(1)?,
                    status_json: row.get(2)?,
                    occurred_at: row.get(3)?,
                })
            },
        )
        .optional()
        .map_err(sql_error)
}

#[cfg(test)]
mod tests {
    use super::{ManagedAppRunConfigRecord, ManagedAppRunTemplateRecord, ManagedAppStatusRecord};
    use crate::SqliteStorage;

    #[test]
    fn managed_app_facts_survive_storage_reopen() {
        let directory = std::env::temp_dir().join(format!(
            "winwincode-managed-app-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock")
                .as_nanos()
        ));
        let mut storage = SqliteStorage::open(&directory).expect("storage");
        storage
            .save_managed_app_run_config(&ManagedAppRunConfigRecord {
                run_id: "run_A".to_owned(),
                work_run_id: "wr_A".to_owned(),
                repository_binding_id: "rb_A".to_owned(),
                attempt: 1,
                config_json: br#"{"runId":"run_A"}"#.to_vec(),
            })
            .expect("config");
        storage
            .save_managed_app_run_template(&ManagedAppRunTemplateRecord {
                repository_binding_id: "rb_A".to_owned(),
                revision: 1,
                template_json: br#"{"schemaVersion":"winwincode/managed-app-template-v1"}"#
                    .to_vec(),
            })
            .expect("template");
        storage
            .save_managed_app_status(&ManagedAppStatusRecord {
                client_node_id: "node_A".to_owned(),
                run_id: "run_A".to_owned(),
                status_json: br#"{"state":"healthy"}"#.to_vec(),
                occurred_at: "2026-09-18T00:00:00Z".to_owned(),
            })
            .expect("status");
        drop(storage);

        let storage = SqliteStorage::open(&directory).expect("reopen storage");
        assert_eq!(
            storage
                .load_managed_app_run_config("run_A")
                .expect("config read")
                .expect("config row")
                .work_run_id,
            "wr_A"
        );
        assert_eq!(
            storage
                .load_managed_app_run_template("rb_A")
                .expect("template read")
                .expect("template row")
                .revision,
            1
        );
        assert_eq!(
            storage
                .load_managed_app_status("node_A", "run_A")
                .expect("status read")
                .expect("status row")
                .occurred_at,
            "2026-09-18T00:00:00Z"
        );
        drop(storage);
        std::fs::remove_dir_all(directory).expect("cleanup");
    }
}
