// SPDX-License-Identifier: Apache-2.0

use std::{
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use winwincode_connector_slack::{
    SlackInstallationIdentity, SlackRateLimitPort, SlackWebApiMethod,
};
use winwincode_integration_core::model::MAX_SAFE_INTEGER;
use winwincode_integration_core::{
    ConnectorCallError, ConnectorCallErrorKind, IntegrationError, IntegrationErrorKind,
};

const RATE_LIMIT_SCHEMA: &str = r"
PRAGMA foreign_keys = ON;
PRAGMA journal_mode = WAL;
PRAGMA synchronous = FULL;
CREATE TABLE IF NOT EXISTS slack_method_rate_limits (
    workspace_id TEXT NOT NULL,
    app_id TEXT NOT NULL,
    method TEXT NOT NULL CHECK (method IN ('chat.postMessage', 'conversations.history')),
    blocked_until_millis INTEGER NOT NULL CHECK (blocked_until_millis > 0),
    revision INTEGER NOT NULL CHECK (revision > 0),
    PRIMARY KEY (workspace_id, app_id, method)
);
";

/// Community SQLite-backed Slack `Retry-After` floor.
#[derive(Clone, Debug)]
pub struct SlackRateLimitGate {
    database_path: PathBuf,
}

impl SlackRateLimitGate {
    /// Opens the Slack rate-limit database below one private integration directory.
    ///
    /// # Errors
    ///
    /// Returns a stable adapter error when the directory, database, or schema is unavailable.
    pub fn open(data_directory: impl AsRef<Path>) -> Result<Self, IntegrationError> {
        let data_directory = data_directory.as_ref();
        fs::create_dir_all(data_directory).map_err(|_| storage_error())?;
        let database_path = data_directory.join("slack-rate-limits.sqlite3");
        let connection = open_connection(&database_path).map_err(|_| storage_error())?;
        connection
            .execute_batch(RATE_LIMIT_SCHEMA)
            .map_err(|_| storage_error())?;
        Ok(Self { database_path })
    }

    /// Returns the current shared lower-bound delay for one workspace/application/method.
    ///
    /// # Errors
    ///
    /// Fails closed when the clock or durable gate is invalid.
    pub fn retry_after_millis(
        &self,
        installation: &SlackInstallationIdentity,
        method: SlackWebApiMethod,
        now_millis: u64,
    ) -> Result<Option<u64>, ConnectorCallError> {
        <Self as SlackRateLimitPort>::retry_after_millis(self, installation, method, now_millis)
    }
}

impl SlackRateLimitPort for SlackRateLimitGate {
    fn retry_after_millis(
        &self,
        installation: &SlackInstallationIdentity,
        method: SlackWebApiMethod,
        now_millis: u64,
    ) -> Result<Option<u64>, ConnectorCallError> {
        validate_time(now_millis)?;
        let connection = open_connection(&self.database_path).map_err(|_| call_error())?;
        let blocked_until: Option<i64> = connection
            .query_row(
                "SELECT blocked_until_millis FROM slack_method_rate_limits
                 WHERE workspace_id = ?1 AND app_id = ?2 AND method = ?3",
                params![
                    installation.workspace_id().as_str(),
                    installation.app_id().as_str(),
                    method.as_str()
                ],
                |row| row.get(0),
            )
            .optional()
            .map_err(|_| call_error())?;
        blocked_until
            .map(from_sql_millis)
            .transpose()
            .map(|value| value.filter(|blocked_until| *blocked_until > now_millis))
            .map(|value| value.map(|blocked_until| blocked_until - now_millis))
    }

    fn observe(
        &self,
        installation: &SlackInstallationIdentity,
        method: SlackWebApiMethod,
        now_millis: u64,
        retry_after_millis: u64,
    ) -> Result<u64, ConnectorCallError> {
        validate_time(now_millis)?;
        let proposed = now_millis
            .checked_add(retry_after_millis)
            .filter(|value| *value <= MAX_SAFE_INTEGER)
            .ok_or_else(call_error)?;
        let mut connection = open_connection(&self.database_path).map_err(|_| call_error())?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|_| call_error())?;
        let existing: Option<(i64, i64)> = transaction
            .query_row(
                "SELECT blocked_until_millis, revision FROM slack_method_rate_limits
                 WHERE workspace_id = ?1 AND app_id = ?2 AND method = ?3",
                params![
                    installation.workspace_id().as_str(),
                    installation.app_id().as_str(),
                    method.as_str()
                ],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(|_| call_error())?;
        let (blocked_until, revision) =
            existing.map_or(Ok((proposed, 1_u64)), |(stored_until, stored_revision)| {
                let stored_until = from_sql_millis(stored_until)?;
                let stored_revision = from_sql_millis(stored_revision)?;
                Ok((
                    proposed.max(stored_until),
                    stored_revision
                        .checked_add(1)
                        .filter(|value| *value <= MAX_SAFE_INTEGER)
                        .ok_or_else(call_error)?,
                ))
            })?;
        transaction
            .execute(
                "INSERT INTO slack_method_rate_limits
                 (workspace_id, app_id, method, blocked_until_millis, revision)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(workspace_id, app_id, method) DO UPDATE SET
                   blocked_until_millis = excluded.blocked_until_millis,
                   revision = excluded.revision",
                params![
                    installation.workspace_id().as_str(),
                    installation.app_id().as_str(),
                    method.as_str(),
                    to_sql_millis(blocked_until)?,
                    to_sql_millis(revision)?
                ],
            )
            .map_err(|_| call_error())?;
        transaction.commit().map_err(|_| call_error())?;
        Ok(blocked_until - now_millis)
    }
}

fn open_connection(path: &Path) -> rusqlite::Result<Connection> {
    let connection = Connection::open(path)?;
    connection.busy_timeout(Duration::from_secs(5))?;
    connection.execute_batch("PRAGMA foreign_keys = ON; PRAGMA synchronous = FULL;")?;
    Ok(connection)
}

fn validate_time(value: u64) -> Result<(), ConnectorCallError> {
    if value == 0 || value > MAX_SAFE_INTEGER {
        Err(call_error())
    } else {
        Ok(())
    }
}

fn to_sql_millis(value: u64) -> Result<i64, ConnectorCallError> {
    i64::try_from(value).map_err(|_| call_error())
}

fn from_sql_millis(value: i64) -> Result<u64, ConnectorCallError> {
    u64::try_from(value)
        .ok()
        .filter(|value| *value > 0 && *value <= MAX_SAFE_INTEGER)
        .ok_or_else(call_error)
}

fn call_error() -> ConnectorCallError {
    ConnectorCallError::try_new(
        ConnectorCallErrorKind::Retryable,
        "SLACK_RATE_LIMIT_STORAGE",
    )
    .expect("static connector error code")
}

const fn storage_error() -> IntegrationError {
    IntegrationError::new(
        IntegrationErrorKind::Storage,
        "Slack rate-limit storage failed",
    )
}
