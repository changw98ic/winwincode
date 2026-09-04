// SPDX-License-Identifier: Apache-2.0

//! Owner-facing user account administration for the `wwc` CLI.
//!
//! The commands reuse the canonical [`UserAccountService`] from the server
//! crate and the storage crate's `UserAccountLedger`; the CLI never
//! re-implements normalization, hashing, or the CAS update rules. The data
//! directory is the Server product-state directory (`WWC_SERVER_DATA_DIRECTORY`),
//! so a CLI run and a running Server share one durable database.
//!
//! Two boundaries are deliberate:
//!
//! - The CLI never touches browser sessions. Disabling an account only
//!   flips the durable state; session revocation stays a Server
//!   responsibility, so a shared-Database disable becomes visible to
//!   already-logged-in browsers only after the Server restarts or through
//!   an HTTP management endpoint.
//! - One Server has at most one Owner. `create --role owner` is accepted
//!   only while no Owner account exists; the durable `server_initialization`
//!   marker of the browser bootstrap stays owned by the Server.

use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

use rusqlite::{Connection, OpenFlags};
use serde::Serialize;
use winwincode_domain::{UserAccount, UserAccountRole, UserAccountState};
use winwincode_server::{
    StandaloneApplicationClock, SystemStandaloneApplicationClock, UserAccountService,
    UserAccountServiceError, UserAccountServiceErrorKind,
};
use winwincode_storage::SqliteStorage;

/// How long a read probe waits for a concurrent Server writer before the
/// CLI reports the storage as busy. Matches the storage adapter's bound.
const PROBE_BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Secret-free summary of one stored account used in command output.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UserAccountView {
    /// Stable account identity.
    pub user_id: String,
    /// Login name exactly as stored.
    pub username: String,
    /// Administration role.
    pub role: UserAccountRole,
    /// Lifecycle state after the command.
    pub state: UserAccountState,
}

impl UserAccountView {
    fn of(account: &UserAccount) -> Self {
        Self {
            user_id: account.user_id.0.clone(),
            username: account.username.clone(),
            role: account.role,
            state: account.state,
        }
    }
}

/// Human- and JSON-readable result of one user administration command.
///
/// The one-time temporary password appears only in `UserCreated` and
/// `PasswordReset`; it is never persisted or shown again by any later
/// command.
#[derive(Clone, Debug, Serialize)]
#[serde(
    tag = "status",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase"
)]
pub enum UserAdminOutcome {
    /// One account was created together with its first temporary password.
    UserCreated {
        /// The stored account.
        user: UserAccountView,
        /// The only plaintext reveal of the stored Argon2id hash.
        temporary_password: String,
    },
    /// One account password was replaced by a fresh temporary password.
    PasswordReset {
        /// The stored account.
        user: UserAccountView,
        /// The only plaintext reveal of the stored Argon2id hash.
        temporary_password: String,
    },
    /// One account state changed, or was already in the requested state.
    UserUpdated {
        /// The stored account.
        user: UserAccountView,
        /// Whether this command moved the state or found it already applied.
        changed: bool,
    },
    /// The data directory has no Owner yet, so the Server is uninitialized.
    InitializationRequired,
}

/// Failure of one user administration command.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UserAdminError {
    /// No Owner exists in the data directory: finish the one-time
    /// initialization first.
    InitializationRequired,
    /// The command cannot be completed. `code` is stable for scripting.
    Failed {
        /// Stable machine-readable failure code.
        code: &'static str,
        /// Human-readable explanation in the CLI language.
        message: String,
    },
}

impl fmt::Display for UserAdminError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InitializationRequired => {
                formatter.write_str("Server 尚未初始化：该数据目录还没有 Owner")
            }
            Self::Failed { code, message } => write!(formatter, "[{code}] {message}"),
        }
    }
}

impl std::error::Error for UserAdminError {}

/// Owner administration tool bound to one Server product-state directory.
///
/// Every operation opens the canonical `SQLite` product-state database the
/// same way the Server does, so CLI and Server always observe one durable
/// account store.
pub struct UserAccountAdmin {
    data_directory: PathBuf,
    clock: SystemStandaloneApplicationClock,
}

impl UserAccountAdmin {
    /// Binds the administrator to one Server product-state directory.
    /// Directories are created lazily by the first operation.
    #[must_use]
    pub fn open(data_directory: impl Into<PathBuf>) -> Self {
        Self {
            data_directory: data_directory.into(),
            clock: SystemStandaloneApplicationClock,
        }
    }

    /// Creates one account with a freshly generated temporary password.
    ///
    /// `owner` is accepted only while the directory has no Owner account;
    /// `member` requires an Owner to already exist.
    ///
    /// # Errors
    ///
    /// Reports a missing Owner for members, an occupied normalized username,
    /// an existing Owner for `owner`, and storage or hashing failure.
    pub fn create(
        &self,
        username: &str,
        role: UserAccountRole,
    ) -> Result<UserAdminOutcome, UserAdminError> {
        let owner_present = self.owner_present()?;
        if role == UserAccountRole::Owner && owner_present {
            return Err(failed(
                "user.owner-exists",
                "Server 已有 Owner，不能再创建第二个 Owner。",
            ));
        }
        if role == UserAccountRole::Member && !owner_present {
            return Err(UserAdminError::InitializationRequired);
        }
        let temporary_password = generate_temporary_password()?;
        let service = self.service()?;
        let now = self.clock.now_instant();
        let account = match role {
            UserAccountRole::Owner => service.initialize_owner(username, &temporary_password, &now),
            UserAccountRole::Member => {
                service.create_user(username, UserAccountRole::Member, &temporary_password, &now)
            }
        }
        .map_err(|error| service_error(&error))?;
        Ok(UserAdminOutcome::UserCreated {
            user: UserAccountView::of(&account),
            temporary_password,
        })
    }

    /// Activates or disables one account looked up by username.
    ///
    /// Disabling an account never revokes browser sessions from the CLI:
    /// revocation is a Server responsibility. When the CLI and a running
    /// Server share this data directory, the disable reaches already
    /// logged-in browsers only after a Server restart or through an HTTP
    /// management endpoint.
    ///
    /// # Errors
    ///
    /// Reports a missing Owner, an unknown username, a concurrent revision
    /// change, and storage failure.
    pub fn set_state(
        &self,
        username: &str,
        state: UserAccountState,
    ) -> Result<UserAdminOutcome, UserAdminError> {
        self.require_initialized()?;
        let current = self.resolve(username)?;
        if current.state == state {
            return Ok(UserAdminOutcome::UserUpdated {
                user: UserAccountView::of(&current),
                changed: false,
            });
        }
        let updated = self.apply_state(&current, state)?;
        Ok(UserAdminOutcome::UserUpdated {
            user: UserAccountView::of(&updated),
            changed: true,
        })
    }

    /// Replaces one account password with a fresh temporary password. The
    /// previous password stops verifying immediately.
    ///
    /// # Errors
    ///
    /// Reports a missing Owner, an unknown username, a concurrent revision
    /// change, and storage or hashing failure.
    pub fn reset_password(&self, username: &str) -> Result<UserAdminOutcome, UserAdminError> {
        self.require_initialized()?;
        let current = self.resolve(username)?;
        let temporary_password = generate_temporary_password()?;
        let service = self.service()?;
        let now = self.clock.now_instant();
        let updated = service
            .set_password(
                &current.user_id,
                &current.revision,
                &temporary_password,
                &now,
            )
            .map_err(|error| service_error(&error))?;
        Ok(UserAdminOutcome::PasswordReset {
            user: UserAccountView::of(&updated),
            temporary_password,
        })
    }

    /// Applies one state change under the observed revision expectation.
    fn apply_state(
        &self,
        current: &UserAccount,
        state: UserAccountState,
    ) -> Result<UserAccount, UserAdminError> {
        let service = self.service()?;
        let now = self.clock.now_instant();
        service
            .set_state(&current.user_id, &current.revision, state, &now)
            .map_err(|error| service_error(&error))
    }

    /// Resolves one username to its stored account through the canonical
    /// normalization and the ledger's normalized-username lookup.
    fn resolve(&self, username: &str) -> Result<UserAccount, UserAdminError> {
        let normalized = UserAccountService::normalize_username(username)
            .map_err(|error| service_error(&error))?;
        self.with_ledger(|ledger| {
            ledger
                .find_by_normalized_username(&normalized)
                .map_err(|error| store_error(&error))?
                .ok_or_else(|| {
                    failed(
                        "user.not-found",
                        format!(
                            "用户不存在：{}",
                            UserAccountService::stored_username(username)
                        ),
                    )
                })
        })
    }

    /// Refuses every command while the directory has no Owner account.
    fn require_initialized(&self) -> Result<(), UserAdminError> {
        if self.owner_present()? {
            return Ok(());
        }
        Err(UserAdminError::InitializationRequired)
    }

    /// Reads whether an Owner account exists directly from the durable
    /// account store. The ledger exposes no role enumeration, so this one
    /// read-only probe queries the ledger-owned `users` table the storage
    /// crate itself created and schema-validates.
    fn owner_present(&self) -> Result<bool, UserAdminError> {
        let storage = self.storage()?;
        let connection =
            Connection::open_with_flags(storage.database_path(), OpenFlags::SQLITE_OPEN_READ_ONLY)
                .map_err(|error| {
                    failed(
                        "user.storage-failed",
                        format!(
                            "无法只读打开产品状态数据库 {}：{error}",
                            storage.database_path().display()
                        ),
                    )
                })?;
        connection
            .busy_timeout(PROBE_BUSY_TIMEOUT)
            .map_err(|error| failed("user.storage-failed", format!("无法设置忙等时限：{error}")))?;
        let users_table: i64 = connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master
                 WHERE type = 'table' AND name = 'users')",
                [],
                |row| row.get(0),
            )
            .map_err(|error| {
                failed(
                    "user.storage-failed",
                    format!("无法读取账户表清单：{error}"),
                )
            })?;
        if users_table != 1 {
            return Ok(false);
        }
        let owner_rows: i64 = connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM users WHERE role = 'owner')",
                [],
                |row| row.get(0),
            )
            .map_err(|error| failed("user.storage-failed", format!("无法读取账户表：{error}")))?;
        Ok(owner_rows == 1)
    }

    /// Opens the canonical product-state database for one ledger operation.
    fn with_ledger<T>(
        &self,
        operation: impl FnOnce(
            &mut winwincode_storage::UserAccountLedger<'_>,
        ) -> Result<T, UserAdminError>,
    ) -> Result<T, UserAdminError> {
        let mut storage = self.storage()?;
        let mut ledger = storage
            .user_account_ledger()
            .map_err(|error| store_error(&error))?;
        operation(&mut ledger)
    }

    /// Opens the canonical account authority on the same directory.
    fn service(&self) -> Result<UserAccountService, UserAdminError> {
        UserAccountService::open(&self.data_directory).map_err(|error| {
            failed(
                "user.storage-failed",
                format!(
                    "无法打开账户服务 {}：{error}",
                    self.data_directory.display()
                ),
            )
        })
    }

    /// Opens the canonical product-state database on the same directory.
    fn storage(&self) -> Result<SqliteStorage, UserAdminError> {
        SqliteStorage::open(&self.data_directory).map_err(|error| {
            failed(
                "user.storage-failed",
                format!(
                    "无法打开产品状态数据库 {}：{error}",
                    self.data_directory.display()
                ),
            )
        })
    }
}

/// Generates one random 20-character temporary password from the same
/// lowercase/digit alphabet as the server-side generator. It is the only
/// plaintext source of the stored Argon2id hash and is returned exactly
/// once to the caller.
///
/// The server crate does not yet export its generator, so the CLI
/// reproduces the shape locally on the same `getrandom` primitive.
///
/// # Errors
///
/// Reports entropy failure.
pub fn generate_temporary_password() -> Result<String, UserAdminError> {
    const ALPHABET: &[u8; 36] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    const LENGTH: usize = 20;
    let mut random = [0_u8; LENGTH];
    getrandom::fill(&mut random)
        .map_err(|_| failed("user.entropy-failed", "无法生成临时密码的随机源。"))?;
    Ok(random
        .iter()
        .map(|byte| char::from(ALPHABET[usize::from(*byte) % ALPHABET.len()]))
        .collect())
}

fn service_error(error: &UserAccountServiceError) -> UserAdminError {
    let (code, message) = match error.kind() {
        UserAccountServiceErrorKind::InvalidInput => (
            "user.username-invalid",
            format!("用户名或密码不满足要求：{error}"),
        ),
        UserAccountServiceErrorKind::Conflict | UserAccountServiceErrorKind::AlreadyInitialized => {
            (
                "user.username-conflict",
                "用户名已被其他账户占用（按规范化用户名判定）。".to_owned(),
            )
        }
        UserAccountServiceErrorKind::NotFound => ("user.not-found", "用户不存在。".to_owned()),
        UserAccountServiceErrorKind::RevisionConflict => (
            "user.revision-conflict",
            "用户记录已被并发修改（revision 冲突），请重试。".to_owned(),
        ),
        UserAccountServiceErrorKind::AccountDisabled
        | UserAccountServiceErrorKind::InvalidCredentials => {
            ("user.state-conflict", "账户状态不允许该操作。".to_owned())
        }
        UserAccountServiceErrorKind::Storage => {
            ("user.storage-failed", format!("账户存储不可用：{error}"))
        }
    };
    failed(code, message)
}

fn store_error(error: &winwincode_storage::UserAccountStoreError) -> UserAdminError {
    failed("user.storage-failed", format!("账户存储操作失败：{error}"))
}

fn failed(code: &'static str, message: impl Into<String>) -> UserAdminError {
    UserAdminError::Failed {
        code,
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use winwincode_domain::UserId;

    use super::*;

    static SEQUENCE: AtomicU64 = AtomicU64::new(0);

    fn admin(label: &str) -> (UserAccountAdmin, PathBuf) {
        let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let directory = std::env::temp_dir().join(format!(
            "winwincode-cli-user-admin-{label}-{}-{sequence}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&directory);
        (UserAccountAdmin::open(&directory), directory)
    }

    fn created(outcome: UserAdminOutcome) -> (UserAccountView, String) {
        match outcome {
            UserAdminOutcome::UserCreated {
                user,
                temporary_password,
            } => (user, temporary_password),
            other => panic!("expected UserCreated, got {other:?}"),
        }
    }

    fn updated(outcome: UserAdminOutcome) -> (UserAccountView, bool) {
        match outcome {
            UserAdminOutcome::UserUpdated { user, changed } => (user, changed),
            other => panic!("expected UserUpdated, got {other:?}"),
        }
    }

    fn reset(outcome: UserAdminOutcome) -> (UserAccountView, String) {
        match outcome {
            UserAdminOutcome::PasswordReset {
                user,
                temporary_password,
            } => (user, temporary_password),
            other => panic!("expected PasswordReset, got {other:?}"),
        }
    }

    fn service(directory: &std::path::Path) -> UserAccountService {
        UserAccountService::open(directory).expect("account service")
    }

    #[test]
    fn generates_passwords_in_the_canonical_shape() {
        let first = generate_temporary_password().expect("first password");
        let second = generate_temporary_password().expect("second password");
        assert_eq!(first.len(), 20);
        assert_ne!(first, second);
        assert!(
            first
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        );
    }

    #[test]
    fn creates_first_owner_then_refuses_every_second_owner() {
        let (admin, directory) = admin("single-owner");
        let (owner, password) = created(
            admin
                .create("Wen", UserAccountRole::Owner)
                .expect("first owner"),
        );
        assert_eq!(owner.role, UserAccountRole::Owner);
        assert_eq!(owner.username, "Wen");
        assert!(owner.user_id.starts_with("usr_"));

        let verified = service(&directory)
            .verify_credentials("wen", &password)
            .expect("lookup")
            .expect("temporary password verifies");
        assert!(verified.password_hash.starts_with("$argon2id$"));

        for candidate in ["Ada", "ada", "WEN"] {
            let refused = admin.create(candidate, UserAccountRole::Owner).unwrap_err();
            assert_eq!(
                refused,
                UserAdminError::Failed {
                    code: "user.owner-exists",
                    message: "Server 已有 Owner，不能再创建第二个 Owner。".to_owned(),
                }
            );
        }
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn member_creation_requires_owner_and_conflicts_on_duplicate_username() {
        let (admin, directory) = admin("member-gate");
        assert_eq!(
            admin.create("Ada", UserAccountRole::Member).unwrap_err(),
            UserAdminError::InitializationRequired
        );
        created(
            admin
                .create("Wen", UserAccountRole::Owner)
                .expect("first owner"),
        );
        let (member, _) = created(
            admin
                .create("Ada", UserAccountRole::Member)
                .expect("first member"),
        );
        assert_eq!(member.role, UserAccountRole::Member);

        let conflict = admin.create("ada", UserAccountRole::Member).unwrap_err();
        assert!(matches!(
            conflict,
            UserAdminError::Failed {
                code: "user.username-conflict",
                ..
            }
        ));
        let owner_conflict = admin.create("WEN", UserAccountRole::Owner).unwrap_err();
        assert!(matches!(
            owner_conflict,
            UserAdminError::Failed {
                code: "user.owner-exists",
                ..
            }
        ));
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn state_changes_and_resets_cas_the_revision() {
        let (admin, directory) = admin("cas");
        let (owner, first_password) = created(
            admin
                .create("Wen", UserAccountRole::Owner)
                .expect("first owner"),
        );

        // A concurrent writer moves the revision after the CLI observed it;
        // the stale expectation must be refused, not silently applied.
        let observed = admin.resolve("Wen").expect("observed account");
        assert_eq!(observed.user_id.0, owner.user_id);
        let _ = service(&directory)
            .set_password(
                &UserId(observed.user_id.0.clone()),
                &observed.revision,
                "concurrent-rotation-1",
                &admin.clock.now_instant(),
            )
            .expect("concurrent rotation");
        assert!(matches!(
            admin.apply_state(&observed, UserAccountState::Disabled),
            Err(UserAdminError::Failed {
                code: "user.revision-conflict",
                ..
            })
        ));

        let (disabled, changed) = updated(
            admin
                .set_state("Wen", UserAccountState::Disabled)
                .expect("disable on fresh revision"),
        );
        assert_eq!(disabled.state, UserAccountState::Disabled);
        assert!(changed);
        // The concurrent rotation already replaced the original password, so
        // the correct credential now reports the account as disabled.
        assert_eq!(
            service(&directory)
                .verify_credentials("wen", "concurrent-rotation-1")
                .expect("lookup"),
            Err(winwincode_server::CredentialRejection::AccountDisabled)
        );
        assert_eq!(
            service(&directory)
                .verify_credentials("wen", &first_password)
                .expect("lookup"),
            Err(winwincode_server::CredentialRejection::BadPassword)
        );

        // Disabling again finds the state already applied and writes nothing.
        let (still_disabled, changed) = updated(
            admin
                .set_state("wen", UserAccountState::Disabled)
                .expect("idempotent disable"),
        );
        assert_eq!(still_disabled.state, UserAccountState::Disabled);
        assert!(!changed);

        let (enabled, changed) = updated(
            admin
                .set_state("Wen", UserAccountState::Active)
                .expect("enable"),
        );
        assert_eq!(enabled.state, UserAccountState::Active);
        assert!(changed);

        let (_, rotated) = reset(admin.reset_password("Wen").expect("reset"));
        let service = service(&directory);
        assert_eq!(
            service
                .verify_credentials("wen", &first_password)
                .expect("lookup"),
            Err(winwincode_server::CredentialRejection::BadPassword)
        );
        assert!(
            service
                .verify_credentials("wen", &rotated)
                .expect("lookup")
                .is_ok()
        );
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn admin_commands_require_an_owner_and_known_usernames() {
        let (admin, directory) = admin("guidance");
        assert_eq!(
            admin
                .set_state("Ada", UserAccountState::Disabled)
                .unwrap_err(),
            UserAdminError::InitializationRequired
        );
        assert_eq!(
            admin.reset_password("Ada").unwrap_err(),
            UserAdminError::InitializationRequired
        );
        created(
            admin
                .create("Wen", UserAccountRole::Owner)
                .expect("first owner"),
        );
        assert!(matches!(
            admin.set_state("Ghost", UserAccountState::Disabled),
            Err(UserAdminError::Failed {
                code: "user.not-found",
                ..
            })
        ));
        assert!(matches!(
            admin.reset_password("Ghost"),
            Err(UserAdminError::Failed {
                code: "user.not-found",
                ..
            })
        ));
        let _ = std::fs::remove_dir_all(directory);
    }
}
