use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use rusqlite::params;
use winwincode_domain::{Instant, Revision, UserAccount, UserId};
use winwincode_storage::{SqliteStorage, UserAccountStoreErrorKind};

static NEXT_TEMP_DIRECTORY: AtomicU64 = AtomicU64::new(1);

const PHC_HASH: &str =
    "$argon2id$v=19$m=19456,t=2,p=1$c29tZXNhbHQ$RdescudvJCsgt3ub+b+dWRWJTmaaJObG";
const OTHER_PHC_HASH: &str =
    "$argon2id$v=19$m=65540,t=3,p=4$mt0udVVJSStlcXI$Zx1PIFhaof2+k1LmoGeWHrAfTj0KXkRz";
const LEGACY_SCHEMA: &str = r"
CREATE TABLE users (
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
CREATE UNIQUE INDEX users_by_normalized_username ON users (normalized_username);
";

fn temporary_directory(name: &str) -> PathBuf {
    let suffix = NEXT_TEMP_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "winwincode-owner-account-{name}-{}-{suffix}",
        std::process::id()
    ))
}

fn instant(text: &str) -> Instant {
    Instant(text.to_owned())
}

fn account(seed: u64) -> UserAccount {
    UserAccount::new(
        UserId(format!("usr_{seed:026}")),
        format!("owner-{seed}"),
        format!("owner-{seed}"),
        PHC_HASH.to_owned(),
        instant("2027-05-01T08:00:00.000Z"),
        instant("2027-05-01T08:00:00.000Z"),
        Revision(1),
    )
    .expect("valid Owner account")
}

fn insert_legacy(connection: &rusqlite::Connection, seed: u64, role: &str, state: &str) {
    connection
        .execute(
            "INSERT INTO users
             (user_id, username, normalized_username, password_hash, role, state,
              created_at, updated_at, revision)
             VALUES (?1, ?2, ?2, ?3, ?4, ?5,
                     '2027-05-01T08:00:00.000Z',
                     '2027-05-01T08:00:00.000Z', 1)",
            params![
                format!("usr_{seed:026}"),
                format!("owner-{seed}"),
                PHC_HASH,
                role,
                state
            ],
        )
        .expect("insert legacy account");
}

#[test]
fn owner_round_trips_and_sqlite_rejects_a_second_account() {
    let root = temporary_directory("singleton");
    let mut storage = SqliteStorage::open(&root).expect("storage");
    let owner = account(1);
    storage
        .user_account_ledger()
        .expect("ledger")
        .create(&owner)
        .expect("create Owner");

    let ledger = storage.user_account_ledger().expect("ledger");
    assert_eq!(ledger.owner().expect("load Owner"), Some(owner.clone()));
    assert_eq!(
        ledger.find(&owner.user_id).expect("find by id"),
        Some(owner.clone())
    );
    assert_eq!(
        ledger
            .find_by_normalized_username(&owner.normalized_username)
            .expect("find by username"),
        Some(owner)
    );

    let second = storage
        .user_account_ledger()
        .expect("ledger")
        .create(&account(2))
        .expect_err("second Owner");
    assert_eq!(second.kind(), UserAccountStoreErrorKind::AlreadyInitialized);

    drop(storage);
    fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn password_update_uses_the_owner_revision() {
    let root = temporary_directory("password");
    let mut storage = SqliteStorage::open(&root).expect("storage");
    let owner = account(1);
    let mut ledger = storage.user_account_ledger().expect("ledger");
    ledger.create(&owner).expect("create Owner");

    let stale = ledger
        .set_password_hash(
            &owner.user_id,
            &Revision(99),
            OTHER_PHC_HASH,
            &instant("2027-05-01T08:00:01.000Z"),
        )
        .expect_err("stale revision");
    assert_eq!(stale.kind(), UserAccountStoreErrorKind::RevisionConflict);

    let updated = ledger
        .set_password_hash(
            &owner.user_id,
            &owner.revision,
            OTHER_PHC_HASH,
            &instant("2027-05-01T08:00:01.000Z"),
        )
        .expect("password update");
    assert_eq!(updated.revision, Revision(2));
    assert_eq!(updated.password_hash, OTHER_PHC_HASH);

    drop(storage);
    fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn one_active_legacy_owner_migrates_once() {
    let root = temporary_directory("migrate");
    let storage = SqliteStorage::open(&root).expect("storage");
    let database_path = storage.database_path().to_path_buf();
    drop(storage);
    let connection = rusqlite::Connection::open(&database_path).expect("raw connection");
    connection
        .execute_batch(LEGACY_SCHEMA)
        .expect("legacy schema");
    insert_legacy(&connection, 1, "owner", "active");
    drop(connection);

    let mut storage = SqliteStorage::open(&root).expect("reopen");
    let owner = storage
        .user_account_ledger()
        .expect("migrated ledger")
        .owner()
        .expect("load")
        .expect("Owner");
    assert_eq!(owner.username, "owner-1");
    let connection = rusqlite::Connection::open(storage.database_path()).expect("raw connection");
    let old_table: i64 = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'users')",
            [],
            |row| row.get(0),
        )
        .expect("table probe");
    assert_eq!(old_table, 0);

    drop(connection);
    drop(storage);
    fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn legacy_identity_sets_are_rejected_without_data_loss() {
    let root = temporary_directory("reject-multi");
    let storage = SqliteStorage::open(&root).expect("storage");
    let database_path = storage.database_path().to_path_buf();
    drop(storage);
    let connection = rusqlite::Connection::open(&database_path).expect("raw connection");
    connection
        .execute_batch(LEGACY_SCHEMA)
        .expect("legacy schema");
    insert_legacy(&connection, 1, "owner", "active");
    insert_legacy(&connection, 2, "member", "active");
    drop(connection);

    let mut storage = SqliteStorage::open(&root).expect("reopen");
    let Err(rejected) = storage.user_account_ledger() else {
        panic!("legacy identity set must be rejected");
    };
    assert_eq!(rejected.kind(), UserAccountStoreErrorKind::CorruptState);
    drop(storage);

    let connection = rusqlite::Connection::open(&database_path).expect("raw connection");
    let count: i64 = connection
        .query_row("SELECT COUNT(*) FROM users", [], |row| row.get(0))
        .expect("legacy rows");
    let owner_table: i64 = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master
             WHERE type = 'table' AND name = 'owner_account')",
            [],
            |row| row.get(0),
        )
        .expect("table probe");
    assert_eq!(count, 2);
    assert_eq!(owner_table, 0);

    drop(connection);
    fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn serialized_shape_contains_only_owner_account_fields() {
    let owner = account(1);
    let object = serde_json::to_value(&owner)
        .expect("serialize")
        .as_object()
        .expect("object")
        .clone();
    assert_eq!(object.len(), 7);
    for field in [
        "createdAt",
        "normalizedUsername",
        "passwordHash",
        "revision",
        "updatedAt",
        "userId",
        "username",
    ] {
        assert!(object.contains_key(field), "missing {field}");
    }
}
