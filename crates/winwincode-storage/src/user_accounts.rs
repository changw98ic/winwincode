// SPDX-License-Identifier: Apache-2.0

//! Durable `UserAccount` records for the multi-user login surface.
//!
//! The ledger owns the `users` table and its uniqueness boundary: one unique
//! index over `normalizedUsername` guarantees that two accounts can never
//! claim the same login identity, while the display `username` stays free to
//! preserve its original spelling. Following the product-state ledger
//! pattern, the table is created idempotently when the ledger opens and its
//! schema is validated against the canonical column list.
//!
//! Password hashing stays outside storage: this module persists and compares
//! only the opaque Argon2id PHC string supplied by the authentication layer.

use std::fmt;

use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use winwincode_domain::{
    Instant, Revision, UserAccount, UserAccountError, UserAccountRole, UserAccountState, UserId,
};

use crate::{SqliteStorage, StorageError, require_canonical_public_id};

const USER_SCHEMA: &str = r"
CREATE TABLE IF NOT EXISTS users (
    user_id TEXT PRIMARY KEY NOT NULL,
    username TEXT NOT NULL CHECK (length(username) > 0),
    normalized_username TEXT NOT NULL CHECK (length(normalized_username) > 0),
    password_hash TEXT NOT NULL CHECK (length(password_hash) > 0),
    role TEXT NOT NULL CHECK (role IN ('owner', 'member')),
    state TEXT NOT NULL CHECK (state IN ('active', 'disabled')),
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL CHECK (updated_at >= created_at),
    revision INTEGER NOT NULL CHECK (revision > 0)
);
CREATE UNIQUE INDEX IF NOT EXISTS users_by_normalized_username
    ON users (normalized_username);
";

/// Stable failure categories exposed by the user account ledger.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UserAccountStoreErrorKind {
    /// Rejected input, such as a malformed identifier or password hash.
    InvalidInput,
    /// No durable account matches the requested identity.
    NotFound,
    /// `normalizedUsername` already belongs to another account.
    NormalizedUsernameConflict,
    /// `userId` already belongs to another account.
    UserIdConflict,
    /// The account changed after the supplied revision expectation.
    RevisionConflict,
    /// A durable row no longer satisfies its canonical invariants.
    CorruptState,
    /// The storage operation itself failed.
    Storage,
}

/// Secret-free user account ledger failure.
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

/// User account ledger borrowing the sole product-state `SQLite` authority.
pub struct UserAccountLedger<'storage> {
    storage: &'storage mut SqliteStorage,
}

impl SqliteStorage {
    /// Opens durable user accounts on this same product-state database.
    ///
    /// # Errors
    ///
    /// Rejects an unavailable connection or an incompatible existing schema.
    pub fn user_account_ledger(&mut self) -> Result<UserAccountLedger<'_>, UserAccountStoreError> {
        UserAccountLedger::new(self)
    }
}

impl<'storage> UserAccountLedger<'storage> {
    fn new(storage: &'storage mut SqliteStorage) -> Result<Self, UserAccountStoreError> {
        let connection = storage
            .connection()
            .map_err(|storage| storage_error(&storage))?;
        connection
            .execute_batch(USER_SCHEMA)
            .map_err(|sql| sql_error(&sql))?;
        validate_schema(connection)?;
        Ok(Self { storage })
    }

    /// Inserts one fully-formed account or reports the exact uniqueness
    /// conflict.
    ///
    /// # Errors
    ///
    /// Rejects an invalid account value, a duplicated `normalizedUsername`,
    /// a duplicated `userId`, or storage failure.
    pub fn create(&mut self, account: &UserAccount) -> Result<UserAccount, UserAccountStoreError> {
        let account = validate(account)?;
        let transaction = self.transaction()?;
        let inserted = transaction
            .execute(
                "INSERT INTO users
                 (user_id, username, normalized_username, password_hash, role, state,
                  created_at, updated_at, revision)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    &account.user_id.0,
                    &account.username,
                    &account.normalized_username,
                    &account.password_hash,
                    account.role.as_str(),
                    account.state.as_str(),
                    &account.created_at.0,
                    &account.updated_at.0,
                    account.revision.0,
                ],
            )
            .map_err(|sql| map_insert_sql(&sql))?;
        if inserted != 1 {
            return Err(error(
                UserAccountStoreErrorKind::Storage,
                "user account insert did not store exactly one row",
            ));
        }
        transaction.commit().map_err(|sql| sql_error(&sql))?;
        Ok(account)
    }

    /// Loads one account by exact user identity.
    ///
    /// # Errors
    ///
    /// Rejects a non-canonical user identity, corrupt rows, or storage
    /// failure.
    pub fn find(&self, user_id: &UserId) -> Result<Option<UserAccount>, UserAccountStoreError> {
        require_canonical_user_id(user_id)?;
        load_by_user_id(
            self.storage
                .connection()
                .map_err(|storage| storage_error(&storage))?,
            user_id,
        )
    }

    /// Loads one account by exact normalized username.
    ///
    /// # Errors
    ///
    /// Rejects an empty normalized username, corrupt rows, or storage
    /// failure.
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
                "SELECT user_id, username, normalized_username, password_hash, role, state,
                        created_at, updated_at, revision
                 FROM users WHERE normalized_username = ?1",
                [normalized_username],
                read_user_row,
            )
            .optional()
            .map_err(|sql| sql_error(&sql))?
            .map(restore_user_parts)
            .transpose()
    }

    /// Replaces the password hash under an exact revision expectation.
    ///
    /// # Errors
    ///
    /// Rejects a stale revision expectation, a password hash that is not an
    /// Argon2id PHC string, a missing account, or storage failure.
    pub fn set_password_hash(
        &mut self,
        user_id: &UserId,
        expected_revision: &Revision,
        password_hash: &str,
        updated_at: &Instant,
    ) -> Result<UserAccount, UserAccountStoreError> {
        require_canonical_user_id(user_id)?;
        require_positive_revision(expected_revision)?;
        self.cas_update(user_id, expected_revision, |current, next_revision| {
            build_updated(
                current,
                password_hash.to_owned(),
                current.state,
                updated_at,
                next_revision,
            )
        })
    }

    /// Activates or disables one account under an exact revision
    /// expectation.
    ///
    /// # Errors
    ///
    /// Rejects a stale revision expectation, a missing account, or storage
    /// failure.
    pub fn set_state(
        &mut self,
        user_id: &UserId,
        expected_revision: &Revision,
        state: UserAccountState,
        updated_at: &Instant,
    ) -> Result<UserAccount, UserAccountStoreError> {
        require_canonical_user_id(user_id)?;
        require_positive_revision(expected_revision)?;
        self.cas_update(user_id, expected_revision, |current, next_revision| {
            build_updated(
                current,
                current.password_hash.clone(),
                state,
                updated_at,
                next_revision,
            )
        })
    }

    fn cas_update<F>(
        &mut self,
        user_id: &UserId,
        expected_revision: &Revision,
        build: F,
    ) -> Result<UserAccount, UserAccountStoreError>
    where
        F: FnOnce(&UserAccount, Revision) -> Result<UserAccount, UserAccountStoreError>,
    {
        let transaction = self.transaction()?;
        let current = require_user(&transaction, user_id)?;
        if current.revision != *expected_revision {
            return Err(error(
                UserAccountStoreErrorKind::RevisionConflict,
                "user account revision differs from the expected revision",
            ));
        }
        let next_revision = Revision(current.revision.0.checked_add(1).ok_or_else(|| {
            error(
                UserAccountStoreErrorKind::CorruptState,
                "stored user account revision overflowed",
            )
        })?);
        let updated = build(&current, next_revision)?;
        let changed = transaction
            .execute(
                "UPDATE users
                 SET password_hash = ?2, state = ?3, updated_at = ?4, revision = ?5
                 WHERE user_id = ?1 AND revision = ?6",
                params![
                    &user_id.0,
                    &updated.password_hash,
                    updated.state.as_str(),
                    &updated.updated_at.0,
                    updated.revision.0,
                    expected_revision.0,
                ],
            )
            .map_err(|sql| sql_error(&sql))?;
        if changed != 1 {
            return Err(error(
                UserAccountStoreErrorKind::RevisionConflict,
                "user account revision changed during the update",
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

fn read_user_row(row: &rusqlite::Row<'_>) -> Result<StoredUserRow, rusqlite::Error> {
    Ok(StoredUserRow {
        user_id: row.get(0)?,
        username: row.get(1)?,
        normalized_username: row.get(2)?,
        password_hash: row.get(3)?,
        role: row.get(4)?,
        state: row.get(5)?,
        created_at: Instant(row.get::<_, String>(6)?),
        updated_at: Instant(row.get::<_, String>(7)?),
        revision: row.get(8)?,
    })
}

struct StoredUserRow {
    user_id: String,
    username: String,
    normalized_username: String,
    password_hash: String,
    role: String,
    state: String,
    created_at: Instant,
    updated_at: Instant,
    revision: i64,
}

fn restore_user_parts(parts: StoredUserRow) -> Result<UserAccount, UserAccountStoreError> {
    restore_user(
        parts.user_id,
        parts.username,
        parts.normalized_username,
        parts.password_hash,
        &parts.role,
        &parts.state,
        parts.created_at,
        parts.updated_at,
        parts.revision,
    )
}

#[allow(clippy::too_many_arguments)]
fn restore_user(
    user_id: String,
    username: String,
    normalized_username: String,
    password_hash: String,
    role: &str,
    state: &str,
    created_at: Instant,
    updated_at: Instant,
    revision: i64,
) -> Result<UserAccount, UserAccountStoreError> {
    let role = UserAccountRole::parse(role).ok_or_else(|| {
        error(
            UserAccountStoreErrorKind::CorruptState,
            "stored user account role is invalid",
        )
    })?;
    let state = UserAccountState::parse(state).ok_or_else(|| {
        error(
            UserAccountStoreErrorKind::CorruptState,
            "stored user account state is invalid",
        )
    })?;
    UserAccount::new(
        UserId(user_id),
        username,
        normalized_username,
        password_hash,
        role,
        state,
        created_at,
        updated_at,
        Revision(revision),
    )
    .map_err(|domain| {
        error(
            UserAccountStoreErrorKind::CorruptState,
            format!("stored user account is invalid: {domain}"),
        )
    })
}

fn load_by_user_id(
    connection: &Connection,
    user_id: &UserId,
) -> Result<Option<UserAccount>, UserAccountStoreError> {
    connection
        .query_row(
            "SELECT user_id, username, normalized_username, password_hash, role, state,
                    created_at, updated_at, revision
             FROM users WHERE user_id = ?1",
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
            "user account does not exist",
        )
    })
}

fn build_updated(
    current: &UserAccount,
    password_hash: String,
    state: UserAccountState,
    updated_at: &Instant,
    next_revision: Revision,
) -> Result<UserAccount, UserAccountStoreError> {
    UserAccount::new(
        current.user_id.clone(),
        current.username.clone(),
        current.normalized_username.clone(),
        password_hash,
        current.role,
        state,
        current.created_at.clone(),
        updated_at.clone(),
        next_revision,
    )
    .map_err(|domain: UserAccountError| {
        error(
            UserAccountStoreErrorKind::InvalidInput,
            format!("user account update is invalid: {domain}"),
        )
    })
}

fn validate(account: &UserAccount) -> Result<UserAccount, UserAccountStoreError> {
    UserAccount::new(
        account.user_id.clone(),
        account.username.clone(),
        account.normalized_username.clone(),
        account.password_hash.clone(),
        account.role,
        account.state,
        account.created_at.clone(),
        account.updated_at.clone(),
        Revision(account.revision.0),
    )
    .map_err(|domain: UserAccountError| {
        error(
            UserAccountStoreErrorKind::InvalidInput,
            format!("user account is invalid: {domain}"),
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

fn validate_schema(connection: &Connection) -> Result<(), UserAccountStoreError> {
    let mut statement = connection
        .prepare("PRAGMA table_info(users)")
        .map_err(|sql| sql_error(&sql))?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|sql| sql_error(&sql))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|sql| sql_error(&sql))?;
    if columns
        != [
            "user_id",
            "username",
            "normalized_username",
            "password_hash",
            "role",
            "state",
            "created_at",
            "updated_at",
            "revision",
        ]
    {
        return Err(error(
            UserAccountStoreErrorKind::CorruptState,
            "user account schema is incompatible",
        ));
    }
    Ok(())
}

fn map_insert_sql(sql: &rusqlite::Error) -> UserAccountStoreError {
    if let rusqlite::Error::SqliteFailure(failure, _) = sql
        && failure.code == rusqlite::ErrorCode::ConstraintViolation
    {
        return match failure.extended_code {
            rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE => error(
                UserAccountStoreErrorKind::NormalizedUsernameConflict,
                "normalizedUsername already belongs to another user account",
            ),
            rusqlite::ffi::SQLITE_CONSTRAINT_PRIMARYKEY => error(
                UserAccountStoreErrorKind::UserIdConflict,
                "userId already belongs to another user account",
            ),
            _ => error(
                UserAccountStoreErrorKind::InvalidInput,
                "user account violates a durable constraint",
            ),
        };
    }
    sql_error(sql)
}

fn storage_error(storage: &StorageError) -> UserAccountStoreError {
    error(
        UserAccountStoreErrorKind::Storage,
        format!("user account storage failed: {storage}"),
    )
}

fn sql_error(_sql: &rusqlite::Error) -> UserAccountStoreError {
    error(
        UserAccountStoreErrorKind::Storage,
        "user account storage operation failed",
    )
}

fn error(kind: UserAccountStoreErrorKind, message: impl Into<String>) -> UserAccountStoreError {
    UserAccountStoreError {
        kind,
        message: message.into(),
    }
}
