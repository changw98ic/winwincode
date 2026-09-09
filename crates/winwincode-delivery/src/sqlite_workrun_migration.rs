// SPDX-License-Identifier: Apache-2.0
//! Durable `SQLite` receipt store for canonical Delivery -> `WorkRun` conversion.
use crate::workrun_migration::{
    WORKRUN_MIGRATION_SCHEMA_VERSION, WorkRunMigrationError, WorkRunMigrationOutcome,
    convert_canonical_delivery,
};
use rusqlite::{
    Connection, OpenFlags, OptionalExtension, Transaction, TransactionBehavior, params,
};
use sha2::{Digest, Sha256};
use std::{
    fs,
    os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _},
    path::Path,
    time::Duration,
};

const DATABASE_MODE: u32 = 0o600;
const DIRECTORY_MODE: u32 = 0o700;
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
const SCHEMA: &str = r"
CREATE TABLE IF NOT EXISTS workrun_migration_sources (
    source_key TEXT PRIMARY KEY NOT NULL,
    schema_version TEXT NOT NULL,
    input_sha256 TEXT NOT NULL,
    consumed INTEGER NOT NULL DEFAULT 1,
    CHECK (length(source_key) BETWEEN 1 AND 512),
    CHECK (schema_version = 'winwincode.delivery-canonical-to-workrun.v1'),
    CHECK (length(input_sha256) = 64),
    CHECK (consumed = 1)
) STRICT;
CREATE TABLE IF NOT EXISTS workrun_migration_snapshots (
    source_key TEXT PRIMARY KEY NOT NULL,
    schema_version TEXT NOT NULL,
    input_sha256 TEXT NOT NULL,
    canonical_snapshot BLOB NOT NULL,
    canonical_sha256 TEXT NOT NULL,
    FOREIGN KEY (source_key) REFERENCES workrun_migration_sources(source_key)
        ON UPDATE RESTRICT ON DELETE RESTRICT,
    CHECK (schema_version = 'winwincode.delivery-canonical-to-workrun.v1'),
    CHECK (length(input_sha256) = 64),
    CHECK (length(canonical_snapshot) > 0),
    CHECK (length(canonical_sha256) = 64)
) STRICT;
";

/// Durable `SQLite` store for the one-time canonical Delivery -> `WorkRun` cutover.
pub struct SqliteWorkRunMigration {
    connection: Connection,
}

impl SqliteWorkRunMigration {
    /// # Errors
    ///
    /// Returns an error if the path cannot be secured, opened, or initialized.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, WorkRunMigrationError> {
        let path = path.as_ref();
        prepare_parent_directory(path)?;
        ensure_database_file(path)?;
        let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE)
            .map_err(|error| WorkRunMigrationError::Transaction(error.to_string()))?;
        Self::initialize(connection)
    }

    /// # Errors
    ///
    /// Returns an error if the in-memory database cannot be initialized.
    pub fn open_in_memory() -> Result<Self, WorkRunMigrationError> {
        let connection = Connection::open_in_memory()
            .map_err(|error| WorkRunMigrationError::Transaction(error.to_string()))?;
        Self::initialize(connection)
    }

    fn initialize(connection: Connection) -> Result<Self, WorkRunMigrationError> {
        connection
            .busy_timeout(BUSY_TIMEOUT)
            .map_err(|error| WorkRunMigrationError::Transaction(error.to_string()))?;
        connection
            .pragma_update(None, "foreign_keys", "ON")
            .map_err(|error| WorkRunMigrationError::Transaction(error.to_string()))?;
        connection
            .pragma_update(None, "journal_mode", "WAL")
            .map_err(|error| WorkRunMigrationError::Transaction(error.to_string()))?;
        connection
            .pragma_update(None, "synchronous", "FULL")
            .map_err(|error| WorkRunMigrationError::Transaction(error.to_string()))?;
        connection
            .execute_batch(SCHEMA)
            .map_err(|error| WorkRunMigrationError::Transaction(error.to_string()))?;
        Ok(Self { connection })
    }

    /// # Errors
    ///
    /// Returns an error if conversion fails, a receipt is inconsistent, or the transaction fails.
    pub fn migrate(
        &mut self,
        input: &[u8],
    ) -> Result<WorkRunMigrationOutcome, WorkRunMigrationError> {
        let (source_key, converted) = convert_canonical_delivery(input)?;
        let input_sha256 = sha256(input);
        let output_sha256 = sha256(&converted);
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(transaction_error)?;

        match durable_snapshot(&transaction, &source_key)? {
            Some(record) => {
                validate_record(&record, &source_key, &input_sha256, &converted)?;
                return Ok(WorkRunMigrationOutcome::AlreadyConsumed {
                    source_key,
                    canonical_snapshot: record.snapshot,
                });
            }
            None => reject_partial_source(&transaction, &source_key)?,
        }

        transaction
            .execute(
                "INSERT INTO workrun_migration_sources
                    (source_key, schema_version, input_sha256, consumed)
                 VALUES (?1, ?2, ?3, 1)",
                params![source_key, WORKRUN_MIGRATION_SCHEMA_VERSION, input_sha256],
            )
            .map_err(transaction_error)?;
        transaction
            .execute(
                "INSERT INTO workrun_migration_snapshots
                    (source_key, schema_version, input_sha256, canonical_snapshot, canonical_sha256)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    source_key,
                    WORKRUN_MIGRATION_SCHEMA_VERSION,
                    input_sha256,
                    converted,
                    output_sha256
                ],
            )
            .map_err(transaction_error)?;
        transaction.commit().map_err(transaction_error)?;
        Ok(WorkRunMigrationOutcome::Applied {
            source_key,
            canonical_snapshot: converted,
        })
    }
}

struct DurableSnapshot {
    schema_version: String,
    snapshot_schema_version: String,
    source_input_sha256: String,
    snapshot_input_sha256: String,
    snapshot: Vec<u8>,
    snapshot_sha256: String,
}

fn durable_snapshot(
    transaction: &Transaction<'_>,
    source_key: &str,
) -> Result<Option<DurableSnapshot>, WorkRunMigrationError> {
    transaction
        .query_row(
            "SELECT sources.schema_version, sources.input_sha256,
                    snapshots.schema_version, snapshots.input_sha256,
                    snapshots.canonical_snapshot, snapshots.canonical_sha256
               FROM workrun_migration_sources AS sources
               JOIN workrun_migration_snapshots AS snapshots
                 ON snapshots.source_key = sources.source_key
              WHERE sources.source_key = ?1
                AND sources.consumed = 1",
            [source_key],
            |row| {
                Ok(DurableSnapshot {
                    schema_version: row.get(0)?,
                    snapshot_schema_version: row.get(2)?,
                    source_input_sha256: row.get(1)?,
                    snapshot_input_sha256: row.get(3)?,
                    snapshot: row.get(4)?,
                    snapshot_sha256: row.get(5)?,
                })
            },
        )
        .optional()
        .map_err(transaction_error)
}

fn validate_record(
    record: &DurableSnapshot,
    source_key: &str,
    input_sha256: &str,
    expected_snapshot: &[u8],
) -> Result<(), WorkRunMigrationError> {
    if record.schema_version != WORKRUN_MIGRATION_SCHEMA_VERSION
        || record.snapshot_schema_version != WORKRUN_MIGRATION_SCHEMA_VERSION
        || record.source_input_sha256 != record.snapshot_input_sha256
        || record.source_input_sha256 != input_sha256
        || record.snapshot != expected_snapshot
        || sha256(&record.snapshot) != record.snapshot_sha256
    {
        return Err(WorkRunMigrationError::CorruptState(format!(
            "inconsistent receipt for source {source_key}"
        )));
    }
    Ok(())
}

fn reject_partial_source(
    transaction: &Transaction<'_>,
    source_key: &str,
) -> Result<(), WorkRunMigrationError> {
    let source_count: i64 = transaction
        .query_row(
            "SELECT count(*) FROM workrun_migration_sources WHERE source_key = ?1",
            [source_key],
            |row| row.get(0),
        )
        .map_err(transaction_error)?;
    let snapshot_count: i64 = transaction
        .query_row(
            "SELECT count(*) FROM workrun_migration_snapshots WHERE source_key = ?1",
            [source_key],
            |row| row.get(0),
        )
        .map_err(transaction_error)?;
    if source_count != 0 || snapshot_count != 0 {
        return Err(WorkRunMigrationError::CorruptState(format!(
            "partial receipt for source {source_key}"
        )));
    }
    Ok(())
}

fn prepare_parent_directory(path: &Path) -> Result<(), WorkRunMigrationError> {
    let parent = path.parent().ok_or_else(|| {
        WorkRunMigrationError::InvalidInput("database path requires parent".into())
    })?;
    match fs::symlink_metadata(parent) {
        Ok(metadata) if metadata.file_type().is_dir() => Ok(()),
        Ok(_) => Err(WorkRunMigrationError::Transaction(
            "database parent is not a directory".into(),
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => fs::DirBuilder::new()
            .recursive(true)
            .mode(DIRECTORY_MODE)
            .create(parent)
            .map_err(transaction_error),
        Err(error) => Err(transaction_error(error)),
    }
}

fn ensure_database_file(path: &Path) -> Result<(), WorkRunMigrationError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => Ok(()),
        Ok(_) => Err(WorkRunMigrationError::Transaction(
            "database path is not a regular file".into(),
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(DATABASE_MODE)
                .open(path)
            {
                Ok(_) => Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    ensure_database_file(path)
                }
                Err(error) => Err(transaction_error(error)),
            }
        }
        Err(error) => Err(transaction_error(error)),
    }
}

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn transaction_error(error: impl std::fmt::Display) -> WorkRunMigrationError {
    WorkRunMigrationError::Transaction(error.to_string())
}
