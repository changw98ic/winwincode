// SPDX-License-Identifier: Apache-2.0

//! The `UserAccountService`: the single canonical authority for durable
//! operator accounts used by browser login.
//!
//! The service is a thin secret-free boundary over the storage crate's
//! `UserAccountLedger`: it owns username normalization, Argon2id password
//! hashing, and the first-Owner initialization rule. It never stores or
//! returns a plaintext password; the Argon2id PHC string is the only durable
//! password form.

use std::fmt;
use std::path::Path;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use rusqlite::{Connection, OpenFlags, OptionalExtension};
use winwincode_domain::{
    Instant, Revision, UserAccount, UserAccountRole, UserAccountState, UserId,
};
use winwincode_storage::SqliteStorage;
use winwincode_storage::UserAccountStoreErrorKind;

use crate::password_hash::{hash_password, verify_password};

/// How long one read-only probe waits for a concurrent writer. Matches the
/// storage adapter's busy bound so Server and CLI probes behave alike.
const PROBE_BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Crockford-style identifier alphabet shared with the canonical `usr_`
/// identifier rules in the domain model (no `I`, `L`, `O`, or `U`).
const USER_ID_ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
const USER_ID_SUFFIX_BYTES: usize = 13;
const MAX_USERNAME_BYTES: usize = 96;
const PASSWORD_MIN_BYTES: usize = 8;
const PASSWORD_MAX_BYTES: usize = 256;

/// Stable failure categories exposed by the account authority.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UserAccountServiceErrorKind {
    /// Rejected input, such as a malformed username or password length.
    InvalidInput,
    /// The requested login identity already belongs to another account.
    Conflict,
    /// The server already has its first Owner account.
    AlreadyInitialized,
    /// No account matches the requested identity.
    NotFound,
    /// The username and password pair is wrong.
    InvalidCredentials,
    /// The account exists but is disabled.
    AccountDisabled,
    /// The account changed after the supplied revision expectation.
    RevisionConflict,
    /// The storage operation itself failed.
    Storage,
}

/// Secret-free account authority failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UserAccountServiceError {
    kind: UserAccountServiceErrorKind,
    message: String,
}

impl UserAccountServiceError {
    #[must_use]
    pub const fn kind(&self) -> UserAccountServiceErrorKind {
        self.kind
    }
}

impl fmt::Display for UserAccountServiceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for UserAccountServiceError {}

/// One login attempt outcome for a known account.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CredentialRejection {
    /// No account carries the requested normalized username.
    UnknownAccount,
    /// The password does not verify against the stored Argon2id hash.
    BadPassword,
    /// The credentials verify but the account is disabled.
    AccountDisabled,
}

/// The single canonical authority over durable operator accounts.
pub struct UserAccountService {
    storage: Mutex<SqliteStorage>,
}

impl UserAccountService {
    /// Opens the account authority on the canonical product-state database.
    ///
    /// # Errors
    ///
    /// Reports unavailable durable storage.
    pub fn open(data_directory: impl AsRef<Path>) -> Result<Self, UserAccountServiceError> {
        let directory = data_directory.as_ref();
        std::fs::create_dir_all(directory).map_err(|_| storage_unavailable())?;
        let storage = SqliteStorage::open(directory).map_err(|_| storage_unavailable())?;
        Ok(Self {
            storage: Mutex::new(storage),
        })
    }

    /// Loads one account by exact user identity.
    ///
    /// # Errors
    ///
    /// Reports storage failure.
    pub fn find(&self, user_id: &UserId) -> Result<Option<UserAccount>, UserAccountServiceError> {
        self.with_ledger(|ledger| ledger.find(user_id).map_err(|store| storage_error(&store)))
    }

    /// Loads the durable active Owner's identity, if one exists, through one
    /// read-only probe of the ledger-owned `users` table.
    ///
    /// "One active Owner exists" is the shared initialization authority for
    /// both first-Owner paths: the browser bootstrap writes the account plus
    /// the session-store marker, while the CLI's `create --role owner` writes
    /// the same account without the marker. Both surfaces therefore agree on
    /// one durable fact, and a disabled-only Owner leaves the recovery path
    /// open.
    ///
    /// # Errors
    ///
    /// Reports an unavailable store.
    pub fn active_owner_id(&self) -> Result<Option<UserId>, UserAccountServiceError> {
        let database_path = {
            let guard = self.storage.lock().map_err(|_| storage_unavailable())?;
            guard.database_path().to_path_buf()
        };
        let connection =
            Connection::open_with_flags(&database_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
                .map_err(|_| storage_unavailable())?;
        connection
            .busy_timeout(PROBE_BUSY_TIMEOUT)
            .map_err(|_| storage_unavailable())?;
        let users_table: i64 = connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master
                 WHERE type = 'table' AND name = 'users')",
                [],
                |row| row.get(0),
            )
            .map_err(|_| storage_unavailable())?;
        if users_table != 1 {
            return Ok(None);
        }
        connection
            .query_row(
                "SELECT user_id FROM users WHERE role = 'owner' AND state = 'active' LIMIT 1",
                [],
                |row| Ok(UserId(row.get(0)?)),
            )
            .optional()
            .map_err(|_| storage_unavailable())
    }

    /// Normalizes one raw username: trim first, then lowercase; the
    /// normalized form must not contain any whitespace. The raw username is
    /// stored exactly as supplied.
    ///
    /// # Errors
    ///
    /// Rejects empty, overlong, control-bearing, or whitespace-carrying
    /// normalized names.
    pub fn normalize_username(raw: &str) -> Result<String, UserAccountServiceError> {
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed.len() > MAX_USERNAME_BYTES {
            return Err(error(
                UserAccountServiceErrorKind::InvalidInput,
                "username must be non-empty, unpadded, and at most 96 bytes",
            ));
        }
        let normalized = trimmed.to_lowercase();
        if normalized.is_empty()
            || normalized.bytes().any(|byte| byte.is_ascii_control())
            || normalized.bytes().any(|byte| byte.is_ascii_whitespace())
        {
            return Err(error(
                UserAccountServiceErrorKind::InvalidInput,
                "normalized username must not contain whitespace or control characters",
            ));
        }
        Ok(normalized)
    }

    /// Returns the username as stored: trimmed, original case preserved. The
    /// domain model rejects padding, so callers must store this form.
    #[must_use]
    pub fn stored_username(raw: &str) -> &str {
        raw.trim()
    }

    /// Validates one plaintext password length bound.
    ///
    /// # Errors
    ///
    /// Rejects passwords outside the accepted length window.
    pub fn validate_password(password: &str) -> Result<(), UserAccountServiceError> {
        if password.len() < PASSWORD_MIN_BYTES || password.len() > PASSWORD_MAX_BYTES {
            return Err(error(
                UserAccountServiceErrorKind::InvalidInput,
                "password must be between 8 and 256 bytes",
            ));
        }
        if password
            .bytes()
            .any(|byte: u8| byte.is_ascii_control() || byte.is_ascii_whitespace())
        {
            return Err(error(
                UserAccountServiceErrorKind::InvalidInput,
                "password must not contain whitespace or control characters",
            ));
        }
        Ok(())
    }

    /// Creates the first Owner account with a fresh canonical `usr_` id.
    ///
    /// The per-Server one-shot initialization gate lives in the session
    /// manager's durable marker, and one durable active Owner — from the
    /// browser bootstrap or from the CLI's `create --role owner` — closes
    /// this path permanently. This layer additionally refuses any request
    /// whose normalized username is already taken.
    ///
    /// # Errors
    ///
    /// Rejects an existing active Owner, an occupied normalized username,
    /// invalid input, and storage failure.
    pub fn initialize_owner(
        &self,
        username: &str,
        password: &str,
        now: &Instant,
    ) -> Result<UserAccount, UserAccountServiceError> {
        Self::validate_password(password)?;
        if self.active_owner_id()?.is_some() {
            return Err(error(
                UserAccountServiceErrorKind::AlreadyInitialized,
                "server initialization already completed",
            ));
        }
        let password_hash = hash_password(password).map_err(|()| hashing_unavailable())?;
        self.with_ledger(|ledger| {
            let normalized = Self::normalize_username(username)?;
            if ledger
                .find_by_normalized_username(&normalized)
                .map_err(|store| storage_error(&store))?
                .is_some()
            {
                return Err(error(
                    UserAccountServiceErrorKind::AlreadyInitialized,
                    "server initialization already completed",
                ));
            }
            ledger
                .create(&account(
                    generate_user_id()?,
                    Self::stored_username(username),
                    &normalized,
                    &password_hash,
                    UserAccountRole::Owner,
                    now,
                )?)
                .map_err(|store| match store.kind() {
                    UserAccountStoreErrorKind::NormalizedUsernameConflict => error(
                        UserAccountServiceErrorKind::AlreadyInitialized,
                        "server initialization already completed",
                    ),
                    UserAccountStoreErrorKind::InvalidInput => {
                        error(UserAccountServiceErrorKind::InvalidInput, store.to_string())
                    }
                    _ => storage_error(&store),
                })
        })
    }

    /// Creates one additional account on behalf of the acting Owner.
    ///
    /// # Errors
    ///
    /// Rejects invalid input, an occupied normalized username, and storage
    /// failure.
    pub fn create_user(
        &self,
        username: &str,
        role: UserAccountRole,
        password: &str,
        now: &Instant,
    ) -> Result<UserAccount, UserAccountServiceError> {
        Self::validate_password(password)?;
        let password_hash = hash_password(password).map_err(|()| hashing_unavailable())?;
        self.with_ledger(|ledger| {
            let normalized = Self::normalize_username(username)?;
            ledger
                .create(&account(
                    generate_user_id()?,
                    Self::stored_username(username),
                    &normalized,
                    &password_hash,
                    role,
                    now,
                )?)
                .map_err(|store| match store.kind() {
                    UserAccountStoreErrorKind::NormalizedUsernameConflict => error(
                        UserAccountServiceErrorKind::Conflict,
                        "username already belongs to another account",
                    ),
                    UserAccountStoreErrorKind::InvalidInput => {
                        error(UserAccountServiceErrorKind::InvalidInput, store.to_string())
                    }
                    _ => storage_error(&store),
                })
        })
    }

    /// Verifies one username/password pair for login.
    ///
    /// # Errors
    ///
    /// Reports storage failure; credential mismatch is returned as a rejection
    /// value rather than an error so callers can apply rate limiting.
    pub fn verify_credentials(
        &self,
        username: &str,
        password: &str,
    ) -> Result<Result<UserAccount, CredentialRejection>, UserAccountServiceError> {
        let normalized = Self::normalize_username(username)?;
        let found = self.with_ledger(|ledger| {
            ledger
                .find_by_normalized_username(&normalized)
                .map_err(|store| storage_error(&store))
        })?;
        let Some(account) = found else {
            // Burn one real Argon2id verification so unknown accounts cost the
            // same as a wrong password on a live account.
            let _ = verify_password(password, dummy_hash());
            return Ok(Err(CredentialRejection::UnknownAccount));
        };
        if !verify_password(password, &account.password_hash) {
            return Ok(Err(CredentialRejection::BadPassword));
        }
        if account.state == UserAccountState::Disabled {
            return Ok(Err(CredentialRejection::AccountDisabled));
        }
        Ok(Ok(account))
    }

    /// Replaces one account password under an exact revision expectation.
    ///
    /// # Errors
    ///
    /// Reports invalid input, a stale revision expectation, or storage
    /// failure.
    pub fn set_password(
        &self,
        user_id: &UserId,
        expected_revision: &Revision,
        password: &str,
        now: &Instant,
    ) -> Result<UserAccount, UserAccountServiceError> {
        Self::validate_password(password)?;
        let password_hash = hash_password(password).map_err(|()| hashing_unavailable())?;
        self.with_ledger(|ledger| {
            ledger
                .set_password_hash(user_id, expected_revision, &password_hash, now)
                .map_err(|store| map_cas_error(&store))
        })
    }

    /// Activates or disables one account under an exact revision
    /// expectation.
    ///
    /// # Errors
    ///
    /// Reports a stale revision expectation, a missing account, or storage
    /// failure.
    pub fn set_state(
        &self,
        user_id: &UserId,
        expected_revision: &Revision,
        state: UserAccountState,
        now: &Instant,
    ) -> Result<UserAccount, UserAccountServiceError> {
        self.with_ledger(|ledger| {
            ledger
                .set_state(user_id, expected_revision, state, now)
                .map_err(|store| map_cas_error(&store))
        })
    }

    fn with_ledger<T>(
        &self,
        operation: impl FnOnce(
            &mut winwincode_storage::UserAccountLedger<'_>,
        ) -> Result<T, UserAccountServiceError>,
    ) -> Result<T, UserAccountServiceError> {
        let mut guard = self.storage.lock().map_err(|_| storage_unavailable())?;
        let mut ledger = guard
            .user_account_ledger()
            .map_err(|store| storage_error(&store))?;
        operation(&mut ledger)
    }
}

/// Generates one random temporary password. It is the only plaintext source
/// of the stored Argon2id hash and is returned exactly once to the caller.
///
/// # Errors
///
/// Reports entropy failure.
pub fn generate_temporary_password() -> Result<String, UserAccountServiceError> {
    const ALPHABET: &[u8; 36] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    const LENGTH: usize = 20;
    let mut random = [0_u8; LENGTH];
    getrandom::fill(&mut random).map_err(|_| storage_unavailable())?;
    Ok(random
        .iter()
        .map(|byte| char::from(ALPHABET[usize::from(*byte) % ALPHABET.len()]))
        .collect())
}

/// One stored Argon2id hash of an unknown random password, generated once.
/// Verifying unknown accounts against it keeps timing uniform with real
/// account lookups.
fn dummy_hash() -> &'static str {
    static DUMMY: OnceLock<String> = OnceLock::new();
    DUMMY.get_or_init(|| {
        hash_password("winwincode-unknown-account-timing-input").unwrap_or_default()
    })
}

fn hashing_unavailable() -> UserAccountServiceError {
    error(
        UserAccountServiceErrorKind::Storage,
        "password hashing is unavailable",
    )
}

fn storage_unavailable() -> UserAccountServiceError {
    error(
        UserAccountServiceErrorKind::Storage,
        "user account storage is unavailable",
    )
}

fn account(
    user_id: String,
    username: &str,
    normalized_username: &str,
    password_hash: &str,
    role: UserAccountRole,
    now: &Instant,
) -> Result<UserAccount, UserAccountServiceError> {
    UserAccount::new(
        UserId(user_id),
        username.to_owned(),
        normalized_username.to_owned(),
        password_hash.to_owned(),
        role,
        UserAccountState::Active,
        now.clone(),
        now.clone(),
        Revision(1),
    )
    .map_err(|domain| {
        error(
            UserAccountServiceErrorKind::InvalidInput,
            domain.to_string(),
        )
    })
}

/// Generates one fresh canonical `usr_` identifier.
fn generate_user_id() -> Result<String, UserAccountServiceError> {
    let mut random = [0_u8; USER_ID_SUFFIX_BYTES];
    getrandom::fill(&mut random).map_err(|_| storage_unavailable())?;
    let mut identifier = String::with_capacity(4 + 26);
    identifier.push_str("usr_");
    for byte in random {
        identifier.push(char::from(USER_ID_ALPHABET[usize::from(byte >> 4)]));
        identifier.push(char::from(USER_ID_ALPHABET[usize::from(byte & 0x0f)]));
    }
    Ok(identifier)
}

fn map_cas_error(store: &winwincode_storage::UserAccountStoreError) -> UserAccountServiceError {
    match store.kind() {
        UserAccountStoreErrorKind::RevisionConflict => error(
            UserAccountServiceErrorKind::RevisionConflict,
            "user account revision differs from the expected revision",
        ),
        UserAccountStoreErrorKind::NotFound => error(
            UserAccountServiceErrorKind::NotFound,
            "user account does not exist",
        ),
        UserAccountStoreErrorKind::InvalidInput => {
            error(UserAccountServiceErrorKind::InvalidInput, store.to_string())
        }
        _ => storage_error(store),
    }
}

fn storage_error(store: &winwincode_storage::UserAccountStoreError) -> UserAccountServiceError {
    error(UserAccountServiceErrorKind::Storage, store.to_string())
}

fn error(kind: UserAccountServiceErrorKind, message: impl Into<String>) -> UserAccountServiceError {
    UserAccountServiceError {
        kind,
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn service(label: &str) -> (UserAccountService, std::path::PathBuf) {
        let directory = std::env::temp_dir().join(format!(
            "winwincode-user-accounts-{label}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&directory);
        (
            UserAccountService::open(&directory).expect("account service"),
            directory,
        )
    }

    fn now() -> Instant {
        Instant("2027-05-01T08:00:00.000Z".to_owned())
    }

    #[test]
    fn normalizes_usernames_by_trim_then_lowercase() {
        assert_eq!(
            UserAccountService::normalize_username("  Wen  ").expect("normalized"),
            "wen"
        );
        assert_eq!(
            UserAccountService::normalize_username(" wen").expect("padded"),
            "wen"
        );
        assert_eq!(
            UserAccountService::normalize_username("MixedCase").expect("case"),
            "mixedcase"
        );
        assert_eq!(UserAccountService::stored_username("  Wen  "), "Wen");
        assert!(UserAccountService::normalize_username("we n").is_err());
        assert!(UserAccountService::normalize_username("we\tn").is_err());
        assert!(UserAccountService::normalize_username("").is_err());
    }

    #[test]
    fn initializes_exactly_one_owner_then_reports_already_initialized() {
        let (service, directory) = service("once");
        let owner = service
            .initialize_owner("Wen", "first-owner-password", &now())
            .expect("first owner");
        assert_eq!(owner.role, UserAccountRole::Owner);
        assert_eq!(owner.username, "Wen");
        assert_eq!(owner.normalized_username, "wen");
        assert_eq!(owner.state, UserAccountState::Active);
        assert!(owner.user_id.0.starts_with("usr_"));
        assert_eq!(owner.user_id.0.len(), 30);
        assert!(owner.password_hash.starts_with("$argon2id$v=19$"));

        // A repeated normalized username cannot create a second account; the
        // per-Server one-shot initialization gate lives in the session
        // manager's durable marker (see auth_session tests).
        let conflict = service
            .initialize_owner("WEN", "third-owner-password", &now())
            .expect_err("same username initialization");
        assert_eq!(
            conflict.kind(),
            UserAccountServiceErrorKind::AlreadyInitialized
        );
        assert_eq!(UserAccountService::stored_username("  Ada  "), "Ada");
        drop(service);
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn active_owner_blocks_a_second_owner_from_either_initialization_path() {
        let (service, directory) = service("shared-authority");
        assert_eq!(service.active_owner_id().expect("empty store"), None);
        let owner = service
            .initialize_owner("Wen", "first-owner-password", &now())
            .expect("first owner");
        assert_eq!(
            service.active_owner_id().expect("active owner"),
            Some(owner.user_id.clone())
        );

        // A second Owner under a fresh username is refused exactly like a
        // repeated username: one active Owner closes initialization for both
        // the browser bootstrap and the CLI.
        let refused = service
            .initialize_owner("Ada", "second-owner-password", &now())
            .expect_err("second owner initialization");
        assert_eq!(
            refused.kind(),
            UserAccountServiceErrorKind::AlreadyInitialized
        );

        // A disabled-only Owner no longer blocks bootstrap, keeping the
        // recovery path open.
        let disabled = service
            .set_state(
                &owner.user_id,
                &owner.revision,
                UserAccountState::Disabled,
                &now(),
            )
            .expect("disable");
        assert_eq!(service.active_owner_id().expect("no active owner"), None);
        let recovered = service
            .initialize_owner("Ada", "recovery-owner-password", &now())
            .expect("recovery owner");
        assert_eq!(recovered.role, UserAccountRole::Owner);
        assert_eq!(disabled.state, UserAccountState::Disabled);
        drop(service);
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn verifies_credentials_and_reports_disabled_accounts() {
        let (service, directory) = service("verify");
        service
            .initialize_owner("Wen", "owner-password-1", &now())
            .expect("owner");
        service
            .create_user("Ada", UserAccountRole::Member, "member-password-1", &now())
            .expect("member");

        let owner = service
            .verify_credentials("WEN", "owner-password-1")
            .expect("lookup")
            .expect("verified");
        assert_eq!(owner.username, "Wen");
        assert!(
            service
                .verify_credentials("ada", "member-password-1")
                .expect("lookup")
                .is_ok()
        );
        assert_eq!(
            service
                .verify_credentials("stranger", "whatever-password")
                .expect("lookup"),
            Err(CredentialRejection::UnknownAccount)
        );
        assert_eq!(
            service
                .verify_credentials("ada", "wrong-password")
                .expect("lookup"),
            Err(CredentialRejection::BadPassword)
        );

        let disabled = service
            .set_state(
                &owner.user_id,
                &owner.revision,
                UserAccountState::Disabled,
                &now(),
            )
            .expect("disable");
        assert_eq!(disabled.state, UserAccountState::Disabled);
        assert_eq!(
            service
                .verify_credentials("wen", "owner-password-1")
                .expect("lookup"),
            Err(CredentialRejection::AccountDisabled)
        );
        drop(service);
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn password_reset_and_state_changes_cas_the_revision() {
        let (service, directory) = service("cas");
        let owner = service
            .initialize_owner("Wen", "owner-password-1", &now())
            .expect("owner");
        let stale =
            service.set_password(&owner.user_id, &Revision(99), "rotated-password-1", &now());
        assert!(stale.is_err());
        let rotated = service
            .set_password(
                &owner.user_id,
                &owner.revision,
                "rotated-password-1",
                &now(),
            )
            .expect("rotate");
        assert_eq!(rotated.revision, Revision(2));
        assert!(
            service
                .verify_credentials("wen", "rotated-password-1")
                .expect("lookup")
                .is_ok()
        );
        let disabled = service
            .set_state(
                &owner.user_id,
                &rotated.revision,
                UserAccountState::Disabled,
                &now(),
            )
            .expect("disable");
        assert_eq!(disabled.revision, Revision(3));
        drop(service);
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn generates_distinct_temporary_passwords() {
        let first = generate_temporary_password().expect("first");
        let second = generate_temporary_password().expect("second");
        assert_eq!(first.len(), 20);
        assert_ne!(first, second);
        assert!(
            first
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        );
    }
}
