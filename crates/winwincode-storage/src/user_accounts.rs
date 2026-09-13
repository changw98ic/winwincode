// SPDX-License-Identifier: Apache-2.0

//! Durable storage for the one local Owner account.
//!
//! `SQLite` enforces the singleton rule with a fixed primary key. An older
//! `users` table is migrated only when it is empty or contains exactly one
//! active Owner; any other identity set is left untouched and rejected.

use std::fmt;

use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use winwincode_domain::{Instant, Revision, UserAccount, UserAccountError, UserId};

use crate::{SqliteStorage, StorageError, require_canonical_public_id};

const OWNER_SCHEMA: &str = r"
CREATE TABLE IF NOT EXISTS owner_account (
    singleton INTEGER PRIMARY KEY NOT NULL CHECK (singleton = 1),
    user_id TEXT NOT NULL UNIQUE,
    username TEXT NOT NULL CHECK (length(username) > 0),
    normalized_username TEXT NOT NULL CHECK (length(normalized_username) > 0),
    password_hash TEXT NOT NULL CHECK (length(password_hash) > 0),
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL CHECK (updated_at >= created_at),
    revision INTEGER NOT NULL CHECK (revision > 0)
);
";

const OWNER_COLUMNS: [&str; 8] = [
    "singleton",
    "user_id",
    "username",
    "normalized_username",
    "password_hash",
    "created_at",
    "updated_at",
    "revision",
];

const LEGACY_COLUMNS: [&str; 9] = [
    "user_id",
    "username",
    "normalized_username",
    "password_hash",
    "role",
    "state",
    "created_at",
    "updated_at",
    "revision",
];

/// Stable failure categories exposed by the Owner account ledger.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UserAccountStoreErrorKind {
    /// Rejected input, such as a malformed identifier or password hash.
    InvalidInput,
    /// No durable Owner matches the requested identity.
    NotFound,
    /// The local Owner has already been initialized.
    AlreadyInitialized,
    /// The account changed after the supplied revision expectation.
    RevisionConflict,
    /// Durable account data or schema is not valid for Community.
    CorruptState,
    /// The storage operation itself failed.
    Storage,
}

/// Secret-free Owner account ledger failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UserAccountStoreError {
    kind: UserAccountStoreErrorKind,
    message: String,
}

impl UserAccountStoreError {
    #[must_use]
    pub const fn kind(&self) -> UserAccountStoreErrorKind {
        self.kind
    }
}

impl fmt::Display for UserAccountStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for UserAccountStoreError {}

/// Owner account ledger borrowing the product-state `SQLite` authority.
pub struct UserAccountLedger<'storage> {
    storage: &'storage mut SqliteStorage,
}

impl SqliteStorage {
    /// Opens the local Owner account on this product-state database.
    ///
    /// # Errors
    ///
    /// Rejects unavailable storage, an incompatible schema, or legacy data
    /// containing anything other than zero or one active Owner.
    pub fn user_account_ledger(&mut self) -> Result<UserAccountLedger<'_>, UserAccountStoreError> {
        UserAccountLedger::new(self)
    }
}

impl<'storage> UserAccountLedger<'storage> {
    fn new(storage: &'storage mut SqliteStorage) -> Result<Self, UserAccountStoreError> {
        let connection = storage
            .connection_mut()
            .map_err(|storage| storage_error(&storage))?;
        let owner_exists = table_exists(connection, "owner_account")?;
        let legacy_exists = table_exists(connection, "users")?;
        match (owner_exists, legacy_exists) {
            (true, true) => {
                return Err(error(
                    UserAccountStoreErrorKind::CorruptState,
                    "Owner account storage contains conflicting schemas",
                ));
            }
            (false, true) => migrate_legacy_users(connection)?,
            _ => connection
                .execute_batch(OWNER_SCHEMA)
                .map_err(|sql| sql_error(&sql))?,
        }
        validate_schema(connection, "owner_account", &OWNER_COLUMNS)?;
        Ok(Self { storage })
    }

    /// Stores the local Owner account.
    ///
    /// # Errors
    ///
    /// Rejects invalid input, a second account, or storage failure.
    pub fn create(&mut self, account: &UserAccount) -> Result<UserAccount, UserAccountStoreError> {
        let account = validate(account)?;
        let transaction = self.transaction()?;
        let inserted = transaction
            .execute(
                "INSERT INTO owner_account
                 (singleton, user_id, username, normalized_username, password_hash,
                  created_at, updated_at, revision)
                 VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    &account.user_id.0,
                    &account.username,
                    &account.normalized_username,
                    &account.password_hash,
                    &account.created_at.0,
                    &account.updated_at.0,
                    account.revision.0,
                ],
            )
            .map_err(|sql| map_insert_sql(&sql))?;
        if inserted != 1 {
            return Err(error(
                UserAccountStoreErrorKind::Storage,
                "Owner account insert did not store exactly one row",
            ));
        }
        transaction.commit().map_err(|sql| sql_error(&sql))?;
        Ok(account)
    }

    /// Loads the Owner by exact user identity.
    ///
    /// # Errors
    ///
    /// Rejects a malformed identity, unavailable storage, or corrupt data.
    pub fn find(&self, user_id: &UserId) -> Result<Option<UserAccount>, UserAccountStoreError> {
        require_canonical_user_id(user_id)?;
        load_by_user_id(
            self.storage
                .connection()
                .map_err(|storage| storage_error(&storage))?,
            user_id,
        )
    }

    /// Loads the Owner by normalized username.
    ///
    /// # Errors
    ///
    /// Rejects an empty username, unavailable storage, or corrupt data.
    pub fn find_by_normalized_username(
        &self,
        normalized_username: &str,
    ) -> Result<Option<UserAccount>, UserAccountStoreError> {
        if normalized_username.is_empty() {
            return Err(error(
                UserAccountStoreErrorKind::InvalidInput,
                "normalizedUsername must not be empty",
            ));
        }
        let connection = self
            .storage
            .connection()
            .map_err(|storage| storage_error(&storage))?;
        connection
            .query_row(
                "SELECT user_id, username, normalized_username, password_hash,
                        created_at, updated_at, revision
                 FROM owner_account WHERE singleton = 1 AND normalized_username = ?1",
                [normalized_username],
                read_user_row,
            )
            .optional()
            .map_err(|sql| sql_error(&sql))?
            .map(restore_user_parts)
            .transpose()
    }

    /// Loads the local Owner, if initialization has completed.
    ///
    /// # Errors
    ///
    /// Rejects unavailable storage or corrupt data.
    pub fn owner(&self) -> Result<Option<UserAccount>, UserAccountStoreError> {
        load_owner(
            self.storage
                .connection()
                .map_err(|storage| storage_error(&storage))?,
        )
    }

    /// Replaces the Owner password hash under an exact revision expectation.
    ///
    /// # Errors
    ///
    /// Rejects invalid input, a missing or changed Owner, or storage failure.
    pub fn set_password_hash(
        &mut self,
        user_id: &UserId,
        expected_revision: &Revision,
        password_hash: &str,
        updated_at: &Instant,
    ) -> Result<UserAccount, UserAccountStoreError> {
        require_canonical_user_id(user_id)?;
        require_positive_revision(expected_revision)?;
        let transaction = self.transaction()?;
        let current = require_user(&transaction, user_id)?;
        if current.revision != *expected_revision {
            return Err(error(
                UserAccountStoreErrorKind::RevisionConflict,
                "Owner account revision differs from the expected revision",
            ));
        }
        let next_revision = Revision(current.revision.0.checked_add(1).ok_or_else(|| {
            error(
                UserAccountStoreErrorKind::CorruptState,
                "stored Owner account revision overflowed",
            )
        })?);
        let updated = UserAccount::new(
            current.user_id.clone(),
            current.username.clone(),
            current.normalized_username.clone(),
            password_hash.to_owned(),
            current.created_at.clone(),
            updated_at.clone(),
            next_revision,
        )
        .map_err(|domain| {
            error(
                UserAccountStoreErrorKind::InvalidInput,
                format!("Owner account update is invalid: {domain}"),
            )
        })?;
        let changed = transaction
            .execute(
                "UPDATE owner_account
                 SET password_hash = ?2, updated_at = ?3, revision = ?4
                 WHERE singleton = 1 AND user_id = ?1 AND revision = ?5",
                params![
                    &user_id.0,
                    &updated.password_hash,
                    &updated.updated_at.0,
                    updated.revision.0,
                    expected_revision.0,
                ],
            )
            .map_err(|sql| sql_error(&sql))?;
        if changed != 1 {
            return Err(error(
                UserAccountStoreErrorKind::RevisionConflict,
                "Owner account revision changed during the update",
            ));
        }
        transaction.commit().map_err(|sql| sql_error(&sql))?;
        Ok(updated)
    }

    fn transaction(&mut self) -> Result<Transaction<'_>, UserAccountStoreError> {
        self.storage
            .connection_mut()
            .map_err(|storage| storage_error(&storage))?
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|sql| sql_error(&sql))
    }
}

fn table_exists(connection: &Connection, table: &str) -> Result<bool, UserAccountStoreError> {
    connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
            [table],
            |row| row.get::<_, i64>(0),
        )
        .map(|exists| exists == 1)
        .map_err(|sql| sql_error(&sql))
}

fn migrate_legacy_users(connection: &mut Connection) -> Result<(), UserAccountStoreError> {
    validate_schema(connection, "users", &LEGACY_COLUMNS)?;
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|sql| sql_error(&sql))?;
    let (count, eligible): (i64, i64) = transaction
        .query_row(
            "SELECT COUNT(*),
                    COALESCE(SUM(CASE WHEN role = 'owner' AND state = 'active' THEN 1 ELSE 0 END), 0)
             FROM users",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(|sql| sql_error(&sql))?;
    if count > 1 || count != eligible {
        return Err(error(
            UserAccountStoreErrorKind::CorruptState,
            "Community requires one active Owner; existing account data was left unchanged",
        ));
    }
    transaction
        .execute_batch(OWNER_SCHEMA)
        .map_err(|sql| sql_error(&sql))?;
    transaction
        .execute(
            "INSERT INTO owner_account
             (singleton, user_id, username, normalized_username, password_hash,
              created_at, updated_at, revision)
             SELECT 1, user_id, username, normalized_username, password_hash,
                    created_at, updated_at, revision
             FROM users",
            [],
        )
        .map_err(|sql| sql_error(&sql))?;
    let _ = load_owner(&transaction)?;
    transaction
        .execute_batch("DROP TABLE users")
        .map_err(|sql| sql_error(&sql))?;
    transaction.commit().map_err(|sql| sql_error(&sql))
}

fn read_user_row(row: &rusqlite::Row<'_>) -> Result<StoredUserRow, rusqlite::Error> {
    Ok(StoredUserRow {
        user_id: row.get(0)?,
        username: row.get(1)?,
        normalized_username: row.get(2)?,
        password_hash: row.get(3)?,
        created_at: Instant(row.get::<_, String>(4)?),
        updated_at: Instant(row.get::<_, String>(5)?),
        revision: row.get(6)?,
    })
}

struct StoredUserRow {
    user_id: String,
    username: String,
    normalized_username: String,
    password_hash: String,
    created_at: Instant,
    updated_at: Instant,
    revision: i64,
}

fn restore_user_parts(parts: StoredUserRow) -> Result<UserAccount, UserAccountStoreError> {
    UserAccount::new(
        UserId(parts.user_id),
        parts.username,
        parts.normalized_username,
        parts.password_hash,
        parts.created_at,
        parts.updated_at,
        Revision(parts.revision),
    )
    .map_err(|domain| {
        error(
            UserAccountStoreErrorKind::CorruptState,
            format!("stored Owner account is invalid: {domain}"),
        )
    })
}

fn load_owner(connection: &Connection) -> Result<Option<UserAccount>, UserAccountStoreError> {
    connection
        .query_row(
            "SELECT user_id, username, normalized_username, password_hash,
                    created_at, updated_at, revision
             FROM owner_account WHERE singleton = 1",
            [],
            read_user_row,
        )
        .optional()
        .map_err(|sql| sql_error(&sql))?
        .map(restore_user_parts)
        .transpose()
}

fn load_by_user_id(
    connection: &Connection,
    user_id: &UserId,
) -> Result<Option<UserAccount>, UserAccountStoreError> {
    connection
        .query_row(
            "SELECT user_id, username, normalized_username, password_hash,
                    created_at, updated_at, revision
             FROM owner_account WHERE singleton = 1 AND user_id = ?1",
            [user_id.0.as_str()],
            read_user_row,
        )
        .optional()
        .map_err(|sql| sql_error(&sql))?
        .map(restore_user_parts)
        .transpose()
}

fn require_user(
    connection: &Connection,
    user_id: &UserId,
) -> Result<UserAccount, UserAccountStoreError> {
    load_by_user_id(connection, user_id)?.ok_or_else(|| {
        error(
            UserAccountStoreErrorKind::NotFound,
            "Owner account does not exist",
        )
    })
}

fn validate(account: &UserAccount) -> Result<UserAccount, UserAccountStoreError> {
    UserAccount::new(
        account.user_id.clone(),
        account.username.clone(),
        account.normalized_username.clone(),
        account.password_hash.clone(),
        account.created_at.clone(),
        account.updated_at.clone(),
        Revision(account.revision.0),
    )
    .map_err(|domain: UserAccountError| {
        error(
            UserAccountStoreErrorKind::InvalidInput,
            format!("Owner account is invalid: {domain}"),
        )
    })
}

fn require_canonical_user_id(user_id: &UserId) -> Result<(), UserAccountStoreError> {
    require_canonical_public_id(&user_id.0, "usr_", "userId")
        .map_err(|storage| error(UserAccountStoreErrorKind::InvalidInput, storage.to_string()))
}

fn require_positive_revision(revision: &Revision) -> Result<(), UserAccountStoreError> {
    if revision.0 < 1 {
        return Err(error(
            UserAccountStoreErrorKind::InvalidInput,
            "expected revision must be positive",
        ));
    }
    Ok(())
}

fn validate_schema(
    connection: &Connection,
    table: &str,
    expected: &[&str],
) -> Result<(), UserAccountStoreError> {
    let sql = format!("PRAGMA table_info({table})");
    let mut statement = connection.prepare(&sql).map_err(|sql| sql_error(&sql))?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|sql| sql_error(&sql))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|sql| sql_error(&sql))?;
    if columns != expected {
        return Err(error(
            UserAccountStoreErrorKind::CorruptState,
            "Owner account schema is incompatible",
        ));
    }
    Ok(())
}

fn map_insert_sql(sql: &rusqlite::Error) -> UserAccountStoreError {
    if let rusqlite::Error::SqliteFailure(failure, _) = sql
        && failure.code == rusqlite::ErrorCode::ConstraintViolation
    {
        return error(
            UserAccountStoreErrorKind::AlreadyInitialized,
            "local Owner already exists",
        );
    }
    sql_error(sql)
}

fn storage_error(storage: &StorageError) -> UserAccountStoreError {
    error(
        UserAccountStoreErrorKind::Storage,
        format!("Owner account storage failed: {storage}"),
    )
}

fn sql_error(_sql: &rusqlite::Error) -> UserAccountStoreError {
    error(
        UserAccountStoreErrorKind::Storage,
        "Owner account storage operation failed",
    )
}

fn error(kind: UserAccountStoreErrorKind, message: impl Into<String>) -> UserAccountStoreError {
    UserAccountStoreError {
        kind,
        message: message.into(),
    }
}
