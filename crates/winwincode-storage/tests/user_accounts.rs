use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use rusqlite::params;
use winwincode_domain::{
    Instant, Revision, UserAccount, UserAccountRole, UserAccountState, UserId,
};
use winwincode_storage::{SqliteStorage, UserAccountStoreErrorKind};

static NEXT_TEMP_DIRECTORY: AtomicU64 = AtomicU64::new(1);

const PHC_HASH: &str =
    "$argon2id$v=19$m=19456,t=2,p=1$c29tZXNhbHQ$RdescudvJCsgt3ub+b+dWRWJTmaaJObG";
const OTHER_PHC_HASH: &str =
    "$argon2id$v=19$m=65540,t=3,p=4$mt0udVVJSStlcXI$Zx1PIFhaof2+k1LmoGeWHrAfTj0KXkRz";

fn temporary_directory(name: &str) -> PathBuf {
    let suffix = NEXT_TEMP_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "winwincode-user-accounts-{name}-{}-{suffix}",
        std::process::id()
    ))
}

fn instant(text: &str) -> Instant {
    Instant(text.to_owned())
}

fn account(seed: u64) -> UserAccount {
    UserAccount::new(
        UserId(format!("usr_{seed:026}")),
        format!("user-{seed}"),
        format!("user-{seed}"),
        PHC_HASH.to_owned(),
        UserAccountRole::Member,
        UserAccountState::Active,
        instant("2027-05-01T08:00:00.000Z"),
        instant("2027-05-01T08:00:00.000Z"),
        Revision(1),
    )
    .expect("valid user account")
}

fn rename(account: &UserAccount, username: &str, normalized: &str) -> UserAccount {
    UserAccount::new(
        account.user_id.clone(),
        username.to_owned(),
        normalized.to_owned(),
        account.password_hash.clone(),
        account.role,
        account.state,
        account.created_at.clone(),
        account.updated_at.clone(),
        Revision(account.revision.0),
    )
    .expect("valid renamed user account")
}

#[test]
fn created_account_round_trips_by_id_and_normalized_username() {
    let root = temporary_directory("round-trip");
    let mut storage = SqliteStorage::open(&root).expect("storage");
    let account = account(1);
    let stored = storage
        .user_account_ledger()
        .expect("ledger")
        .create(&account)
        .expect("create");
    assert_eq!(stored, account);

    let ledger = storage.user_account_ledger().expect("ledger");
    let by_id = ledger.find(&account.user_id).expect("find by id");
    assert_eq!(by_id.as_ref(), Some(&account));
    let by_username = ledger
        .find_by_normalized_username(&account.normalized_username)
        .expect("find by normalized username");
    assert_eq!(by_username.as_ref(), Some(&account));
    let missing_id = ledger
        .find(&UserId("usr_99999999999999999999999999".to_owned()))
        .expect("find missing id");
    assert_eq!(missing_id, None);
    let missing_username = ledger
        .find_by_normalized_username("missing")
        .expect("find missing normalized username");
    assert_eq!(missing_username, None);

    let malformed_id = ledger.find(&UserId("not-canonical".to_owned()));
    assert_eq!(
        malformed_id.expect_err("malformed id").kind(),
        UserAccountStoreErrorKind::InvalidInput
    );

    drop(storage);
    fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn normalized_username_uniqueness_rejects_case_variants() {
    let root = temporary_directory("uniqueness");
    let mut storage = SqliteStorage::open(&root).expect("storage");
    let first = rename(&account(1), "Wen", "wen");
    storage
        .user_account_ledger()
        .expect("first ledger")
        .create(&first)
        .expect("first create");

    let same_normalized = rename(&account(2), "WEN", "wen");
    let conflict = storage
        .user_account_ledger()
        .expect("conflict ledger")
        .create(&same_normalized)
        .expect_err("normalized username conflict");
    assert_eq!(
        conflict.kind(),
        UserAccountStoreErrorKind::NormalizedUsernameConflict
    );

    let same_id = rename(&account(1), "Other", "other");
    let id_conflict = storage
        .user_account_ledger()
        .expect("identity ledger")
        .create(&same_id)
        .expect_err("user id conflict");
    assert_eq!(
        id_conflict.kind(),
        UserAccountStoreErrorKind::UserIdConflict
    );

    // The display username itself is not a uniqueness key.
    let same_username_distinct_normalized = rename(&account(3), "Wen", "wen3");
    storage
        .user_account_ledger()
        .expect("third ledger")
        .create(&same_username_distinct_normalized)
        .expect("same username with distinct normalized username");
    let first_after = storage
        .user_account_ledger()
        .expect("reload ledger")
        .find(&first.user_id)
        .expect("reload")
        .expect("first account still present");
    assert_eq!(first_after, first);
    drop(storage);
    fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn users_table_enforces_the_closed_role_and_state_enums() {
    let root = temporary_directory("enums");
    let mut storage = SqliteStorage::open(&root).expect("storage");
    let account = account(1);
    storage
        .user_account_ledger()
        .expect("ledger")
        .create(&account)
        .expect("create");
    let database_path = storage.database_path().to_path_buf();
    drop(storage);

    let connection = rusqlite::Connection::open(&database_path).expect("raw connection");
    let insert_with_state = |state: &str| {
        connection.execute(
            "INSERT INTO users
             (user_id, username, normalized_username, password_hash, role, state,
              created_at, updated_at, revision)
             VALUES ('usr_99999999999999999999999902', 'other', 'other', ?2,
                     'member', ?1, '2027-05-01T08:00:00.000Z',
                     '2027-05-01T08:00:00.000Z', 1)",
            params![state, PHC_HASH],
        )
    };
    assert!(insert_with_state("archived").is_err());
    assert!(insert_with_state("active").is_ok());
    let insert_with_role = |role: &str| {
        connection.execute(
            "INSERT INTO users
             (user_id, username, normalized_username, password_hash, role, state,
              created_at, updated_at, revision)
             VALUES ('usr_99999999999999999999999903', 'third', 'third', ?2,
                     ?1, 'active', '2027-05-01T08:00:00.000Z',
                     '2027-05-01T08:00:00.000Z', 1)",
            params![role, PHC_HASH],
        )
    };
    assert!(insert_with_role("administrator").is_err());
    assert!(insert_with_role("owner").is_ok());
    fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn updates_cas_increment_revision_and_reject_stale_expectations() {
    let root = temporary_directory("revision");
    let mut storage = SqliteStorage::open(&root).expect("storage");
    let account = account(1);
    let mut ledger = storage.user_account_ledger().expect("ledger");
    ledger.create(&account).expect("create");

    let rehashed = ledger
        .set_password_hash(
            &account.user_id,
            &Revision(1),
            OTHER_PHC_HASH,
            &instant("2027-05-01T08:00:01.000Z"),
        )
        .expect("password update");
    assert_eq!(rehashed.revision, Revision(2));
    assert_eq!(rehashed.password_hash, OTHER_PHC_HASH);
    assert_eq!(rehashed.updated_at, instant("2027-05-01T08:00:01.000Z"));
    assert_eq!(rehashed.created_at, account.created_at);

    let disabled = ledger
        .set_state(
            &account.user_id,
            &Revision(2),
            UserAccountState::Disabled,
            &instant("2027-05-01T08:00:02.000Z"),
        )
        .expect("disable");
    assert_eq!(disabled.revision, Revision(3));
    assert_eq!(disabled.state, UserAccountState::Disabled);

    let stale = ledger.set_password_hash(
        &account.user_id,
        &Revision(2),
        PHC_HASH,
        &instant("2027-05-01T08:00:03.000Z"),
    );
    assert_eq!(
        stale.expect_err("stale revision").kind(),
        UserAccountStoreErrorKind::RevisionConflict
    );

    let invalid_hash = ledger.set_password_hash(
        &account.user_id,
        &Revision(3),
        "plain-text-password",
        &instant("2027-05-01T08:00:03.000Z"),
    );
    assert_eq!(
        invalid_hash.expect_err("invalid hash").kind(),
        UserAccountStoreErrorKind::InvalidInput
    );

    let missing = ledger.set_password_hash(
        &UserId("usr_88888888888888888888888888".to_owned()),
        &Revision(1),
        OTHER_PHC_HASH,
        &instant("2027-05-01T08:00:03.000Z"),
    );
    assert_eq!(
        missing.expect_err("missing account").kind(),
        UserAccountStoreErrorKind::NotFound
    );

    let unchanged = ledger
        .find(&account.user_id)
        .expect("reload")
        .expect("account present");
    assert_eq!(unchanged.revision, Revision(3));
    assert_eq!(unchanged.password_hash, OTHER_PHC_HASH);
    assert_eq!(unchanged.state, UserAccountState::Disabled);

    let reactivated = ledger
        .set_state(
            &account.user_id,
            &Revision(3),
            UserAccountState::Active,
            &instant("2027-05-01T08:00:04.000Z"),
        )
        .expect("re-activate");
    assert_eq!(reactivated.revision, Revision(4));
    assert_eq!(reactivated.state, UserAccountState::Active);

    drop(storage);
    fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn serialized_shape_uses_canonical_camel_case_fields() {
    let account = account(1);
    let value = serde_json::to_value(&account).expect("serialize");
    let object = value.as_object().expect("object");
    for name in [
        "userId",
        "username",
        "normalizedUsername",
        "passwordHash",
        "role",
        "state",
        "createdAt",
        "updatedAt",
        "revision",
    ] {
        assert!(object.contains_key(name), "missing field {name}");
    }
    assert_eq!(object["role"], "member");
    assert_eq!(object["state"], "active");
    assert_eq!(object["normalizedUsername"], "user-1");

    let parsed: UserAccount = serde_json::from_value(value.clone()).expect("deserialize");
    assert_eq!(parsed, account);

    let mut unknown_field = object.clone();
    unknown_field.insert("extra".to_owned(), serde_json::Value::Null);
    assert!(
        serde_json::from_value::<UserAccount>(serde_json::Value::Object(unknown_field)).is_err()
    );

    let mut wrong_role = object.clone();
    wrong_role["role"] = serde_json::Value::String("administrator".to_owned());
    assert!(serde_json::from_value::<UserAccount>(serde_json::Value::Object(wrong_role)).is_err());

    let mut wrong_state = object.clone();
    wrong_state["state"] = serde_json::Value::String("archived".to_owned());
    assert!(serde_json::from_value::<UserAccount>(serde_json::Value::Object(wrong_state)).is_err());
}
