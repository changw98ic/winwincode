// SPDX-License-Identifier: Apache-2.0

//! Durable one-time legacy Session identity migration.

use std::fmt;
use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension as _, TransactionBehavior, params};
use sha2::{Digest as _, Sha256};

use crate::migration::{
    MIGRATION_SCHEMA_VERSION, MigrationCommit, MigrationError, MigrationOutcome,
    MigrationTransaction, MigrationTransactionError, migrate_legacy_delivery_json,
};

const DIRECTORY_MODE: u32 = 0o700;
const DATABASE_MODE: u32 = 0o600;
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

const SCHEMA: &str = r"
CREATE TABLE IF NOT EXISTS session_identity_migration_sources (
    source_key TEXT PRIMARY KEY NOT NULL,
    migration_schema_version TEXT NOT NULL,
    CHECK (length(source_key) BETWEEN 1 AND 512),
    CHECK (migration_schema_version = 'winwincode.delivery-strongflow-legacy-to-canonical.v1')
) STRICT;

CREATE TABLE IF NOT EXISTS session_identity_migration_snapshots (
    source_key TEXT PRIMARY KEY NOT NULL,
    canonical_snapshot BLOB NOT NULL,
    canonical_sha256 TEXT NOT NULL,
    FOREIGN KEY (source_key)
        REFERENCES session_identity_migration_sources(source_key)
        ON UPDATE RESTRICT ON DELETE RESTRICT,
    CHECK (length(canonical_snapshot) > 0),
    CHECK (length(canonical_sha256) = 64)
) STRICT;

CREATE TABLE IF NOT EXISTS session_identity_migration_consumed (
    source_key TEXT PRIMARY KEY NOT NULL,
    consumed_marker INTEGER NOT NULL DEFAULT 1,
    FOREIGN KEY (source_key)
        REFERENCES session_identity_migration_snapshots(source_key)
        ON UPDATE RESTRICT ON DELETE RESTRICT,
    CHECK (consumed_marker = 1)
) STRICT;
";

/// Failure opening or initializing the durable Session migration store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqliteSessionIdentityMigrationError {
    message: String,
}

impl SqliteSessionIdentityMigrationError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for SqliteSessionIdentityMigrationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for SqliteSessionIdentityMigrationError {}

/// `SQLite` adapter for the one-time legacy Session identity migration.
///
/// Each successful first migration writes the source marker, canonical
/// snapshot, and consumed marker in one `IMMEDIATE` transaction. A repeated
/// source reads the first snapshot before considering any write.
pub struct SqliteSessionIdentityMigration {
    connection: Connection,
}

impl SqliteSessionIdentityMigration {
    /// Opens or creates a durable migration database.
    ///
    /// The database's parent directory is created with mode `0700` and the
    /// database itself is set to `0600` before it is used.
    ///
    /// # Errors
    ///
    /// Returns an error when the directory, permissions, `SQLite` connection,
    /// durability settings, or closed schema cannot be established.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, SqliteSessionIdentityMigrationError> {
        let path = path.as_ref();
        prepare_parent_directory(path)?;
        let connection = Connection::open(path).map_err(|error| sqlite_open_error(&error))?;
        fs::set_permissions(path, fs::Permissions::from_mode(DATABASE_MODE)).map_err(|error| {
            SqliteSessionIdentityMigrationError::new(format!(
                "failed to secure Session migration database {}: {error}",
                path.display()
            ))
        })?;
        Self::initialize(connection)
    }

    /// Opens an isolated in-memory adapter.
    ///
    /// # Errors
    ///
    /// Returns an error when `SQLite` or the closed schema cannot be initialized.
    pub fn open_in_memory() -> Result<Self, SqliteSessionIdentityMigrationError> {
        let connection = Connection::open_in_memory().map_err(|error| sqlite_open_error(&error))?;
        Self::initialize(connection)
    }

    fn initialize(connection: Connection) -> Result<Self, SqliteSessionIdentityMigrationError> {
        connection
            .busy_timeout(BUSY_TIMEOUT)
            .map_err(|error| sqlite_open_error(&error))?;
        connection
            .pragma_update(None, "foreign_keys", "ON")
            .map_err(|error| sqlite_open_error(&error))?;
        connection
            .pragma_update(None, "journal_mode", "WAL")
            .map_err(|error| sqlite_open_error(&error))?;
        connection
            .pragma_update(None, "synchronous", "FULL")
            .map_err(|error| sqlite_open_error(&error))?;
        connection
            .execute_batch(SCHEMA)
            .map_err(|error| sqlite_open_error(&error))?;
        Ok(Self { connection })
    }

    /// Converts and atomically consumes one complete legacy Delivery snapshot.
    ///
    /// # Errors
    ///
    /// Returns an error when the legacy graph is invalid or the complete
    /// `SQLite` transaction cannot be committed.
    pub fn migrate(&mut self, legacy_snapshot: &[u8]) -> Result<MigrationOutcome, MigrationError> {
        migrate_legacy_delivery_json(legacy_snapshot, self)
    }
}

impl MigrationTransaction for SqliteSessionIdentityMigration {
    fn commit_once(
        &mut self,
        source_key: &str,
        canonical_snapshot: &[u8],
    ) -> Result<MigrationCommit, MigrationTransactionError> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| storage_error(&error))?;

        if let Some((durable_snapshot, durable_sha256)) = transaction
            .query_row(
                "SELECT snapshots.canonical_snapshot, snapshots.canonical_sha256
                   FROM session_identity_migration_consumed AS consumed
                   JOIN session_identity_migration_snapshots AS snapshots
                     ON snapshots.source_key = consumed.source_key
                   JOIN session_identity_migration_sources AS sources
                     ON sources.source_key = consumed.source_key
                  WHERE consumed.source_key = ?1
                    AND consumed.consumed_marker = 1
                    AND sources.migration_schema_version = ?2",
                params![source_key, MIGRATION_SCHEMA_VERSION],
                |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()
            .map_err(|error| storage_error(&error))?
        {
            let actual_sha256 = format!("{:x}", Sha256::digest(&durable_snapshot));
            if actual_sha256 != durable_sha256 {
                return Err(MigrationTransactionError::CorruptState {
                    message: format!("snapshot digest differs for source {source_key}"),
                });
            }
            return Ok(MigrationCommit::AlreadyConsumed {
                canonical_snapshot: durable_snapshot,
            });
        }

        reject_partial_source(&transaction, source_key)?;
        let canonical_sha256 = format!("{:x}", Sha256::digest(canonical_snapshot));
        transaction
            .execute(
                "INSERT INTO session_identity_migration_sources
                    (source_key, migration_schema_version)
                 VALUES (?1, ?2)",
                params![source_key, MIGRATION_SCHEMA_VERSION],
            )
            .map_err(|error| storage_error(&error))?;
        transaction
            .execute(
                "INSERT INTO session_identity_migration_snapshots
                    (source_key, canonical_snapshot, canonical_sha256)
                 VALUES (?1, ?2, ?3)",
                params![source_key, canonical_snapshot, canonical_sha256],
            )
            .map_err(|error| storage_error(&error))?;
        transaction
            .execute(
                "INSERT INTO session_identity_migration_consumed
                    (source_key, consumed_marker)
                 VALUES (?1, 1)",
                [source_key],
            )
            .map_err(|error| storage_error(&error))?;
        transaction
            .commit()
            .map_err(|error| storage_error(&error))?;
        Ok(MigrationCommit::Applied)
    }
}

fn reject_partial_source(
    transaction: &rusqlite::Transaction<'_>,
    source_key: &str,
) -> Result<(), MigrationTransactionError> {
    let record_count = transaction
        .query_row(
            "SELECT
                 (SELECT count(*) FROM session_identity_migration_sources WHERE source_key = ?1) +
                 (SELECT count(*) FROM session_identity_migration_snapshots WHERE source_key = ?1) +
                 (SELECT count(*) FROM session_identity_migration_consumed WHERE source_key = ?1)",
            [source_key],
            |row| row.get::<_, i64>(0),
        )
        .map_err(|error| storage_error(&error))?;
    let record_count =
        u64::try_from(record_count).map_err(|_| MigrationTransactionError::CorruptState {
            message: "negative record count".to_owned(),
        })?;
    if record_count == 0 {
        Ok(())
    } else {
        Err(MigrationTransactionError::CorruptState {
            message: format!("incomplete records exist for source {source_key}"),
        })
    }
}

fn prepare_parent_directory(path: &Path) -> Result<(), SqliteSessionIdentityMigrationError> {
    let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    else {
        return Err(SqliteSessionIdentityMigrationError::new(
            "Session migration database path must include a parent directory",
        ));
    };
    fs::create_dir_all(parent).map_err(|error| {
        SqliteSessionIdentityMigrationError::new(format!(
            "failed to create Session migration directory {}: {error}",
            parent.display()
        ))
    })?;
    fs::set_permissions(parent, fs::Permissions::from_mode(DIRECTORY_MODE)).map_err(|error| {
        SqliteSessionIdentityMigrationError::new(format!(
            "failed to secure Session migration directory {}: {error}",
            parent.display()
        ))
    })
}

fn sqlite_open_error(error: &rusqlite::Error) -> SqliteSessionIdentityMigrationError {
    SqliteSessionIdentityMigrationError::new(format!(
        "failed to initialize Session migration SQLite store: {error}"
    ))
}

fn storage_error(error: &rusqlite::Error) -> MigrationTransactionError {
    MigrationTransactionError::Storage {
        message: error.to_string(),
    }
}
