// SPDX-License-Identifier: Apache-2.0

use super::*;
use serde_json::json;
use std::{
    path::PathBuf,
    process::Command,
    sync::{
        Arc, Barrier,
        atomic::{AtomicUsize, Ordering},
    },
};

struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        Self(std::env::temp_dir().join(format!(
                "wwc-opencode-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            )))
    }
    fn open(&self) -> DeviceProviderStore {
        DeviceProviderStore::open(&self.0).unwrap()
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn grant(label: &str) -> OpenCodeTokenGrant {
    OpenCodeTokenGrant {
        access_token: ResolvedSecret::from_bytes(format!("access-{label}").into_bytes()).unwrap(),
        refresh_token: ResolvedSecret::from_bytes(format!("refresh-{label}").into_bytes()).unwrap(),
        expires_in: Duration::from_hours(1),
    }
}
fn user(subject: &str) -> OpenCodeUser {
    OpenCodeUser {
        id: subject.into(),
        email: format!("{subject}@example.test"),
    }
}
fn config(org: &str) -> serde_json::Value {
    json!({"config":{"provider":{"opencode-go":{"api":"https://opencode.ai/inference/go/openai/v1",
        "npm":"@ai-sdk/openai-compatible","options":{"headers":{"x-opencode-org-id":org}},
        "models":{"glm-5.3-flash":{"tool_call":true}}}}}})
}
fn connect(store: &DeviceProviderStore, account: &OpenCodeAccountId, provider: &str, org: &str) {
    store
        .connect_opencode(
            account,
            &OpenCodeOrganization {
                id: org.into(),
                name: org.into(),
            },
            &config(org),
            provider.into(),
            provider.into(),
        )
        .unwrap();
}
fn expire(store: &DeviceProviderStore, account: &OpenCodeAccountId) {
    store
        .connection
        .execute(
            "UPDATE opencode_accounts SET expires_at_ms=0 WHERE account_ref=?1",
            [&account.0],
        )
        .unwrap();
}

#[test]
fn accounts_deduplicate_by_verified_subject_and_connections_share_credentials() {
    let directory = Directory::new();
    let store = directory.open();
    let a = store
        .save_opencode_account(&user("user-a"), grant("a1"), || true)
        .unwrap();
    let b = store
        .save_opencode_account(&user("user-b"), grant("b1"), || true)
        .unwrap();
    connect(&store, &a, "go-a1", "org-a1");
    connect(&store, &a, "go-a2", "org-a2");
    connect(&store, &b, "go-b", "org-b");
    let again = store
        .save_opencode_account(&user("user-a"), grant("a2"), || true)
        .unwrap();
    assert_eq!(a, again);
    assert_ne!(a, b);
    let accounts = store.opencode_accounts().unwrap();
    assert_eq!(accounts.len(), 2);
    assert_eq!(
        accounts
            .iter()
            .find(|account| account.account_ref == a)
            .unwrap()
            .credential_version,
        2
    );
    assert_eq!(
        store.opencode_access(&a, || true).unwrap().expose(),
        b"access-a2"
    );
    assert_eq!(
        store.opencode_access(&b, || true).unwrap().expose(),
        b"access-b1"
    );
    let snapshot = store.snapshot("device-a").unwrap();
    assert!(
        snapshot
            .providers
            .iter()
            .all(|provider| provider.credential_configured)
    );
    let projection = serde_json::to_string(&snapshot).unwrap();
    assert!(!projection.contains("access-"));
    assert!(!projection.contains("refresh-"));
    assert!(!projection.contains("device_code"));
    assert!(
        snapshot
            .providers
            .iter()
            .all(|provider| provider.open_code.is_some())
    );
    assert!(
        store
            .connect_opencode(
                &b,
                &OpenCodeOrganization {
                    id: "org-b".into(),
                    name: "B".into()
                },
                &config("org-b"),
                "go-a1".into(),
                "changed".into()
            )
            .is_err()
    );
}

#[test]
fn late_auth_rejection_does_not_revoke_a_newer_authorization() {
    let directory = Directory::new();
    let store = directory.open();
    let account = store
        .save_opencode_account(&user("user-a"), grant("old"), || true)
        .unwrap();
    let old = store.opencode_access(&account, || true).unwrap();
    store
        .save_opencode_account(&user("user-a"), grant("new"), || true)
        .unwrap();
    store.reject_opencode_access(&account, &old).unwrap();
    let new = store.opencode_access(&account, || true).unwrap();
    assert_eq!(new.expose(), b"access-new");
    store.reject_opencode_access(&account, &new).unwrap();
    assert!(matches!(
        store.opencode_access(&account, || true),
        Err(OpenCodeCredentialError::ReauthorizationRequired)
    ));
}

#[test]
fn public_projection_rejects_incoherent_account_and_route_metadata() {
    let directory = Directory::new();
    let store = directory.open();
    let account = store
        .save_opencode_account(&user("user-a"), grant("a"), || true)
        .unwrap();
    connect(&store, &account, "go-a", "org-a");
    let original = store.snapshot("device-a").unwrap();
    assert!(crate::valid_opencode_projection(&original));
    let mut forged = original.clone();
    forged.open_code_accounts = None;
    assert!(!crate::valid_opencode_projection(&forged));
    forged = original.clone();
    forged.providers[0].config.endpoint = "https://evil.test".into();
    assert!(!crate::valid_opencode_projection(&forged));
    forged = original.clone();
    forged.providers[0].credential_configured = false;
    assert!(!crate::valid_opencode_projection(&forged));
    forged = original.clone();
    forged.default_provider_id = Some("missing".into());
    assert!(!crate::valid_opencode_projection(&forged));
    forged = original.clone();
    let duplicate = forged.open_code_accounts.as_ref().unwrap()[0].clone();
    forged.open_code_accounts.as_mut().unwrap().push(duplicate);
    assert!(!crate::valid_opencode_projection(&forged));
}

#[test]
fn qwen_connection_retains_oauth_headers_and_protocol_across_store_reopen() {
    let directory = Directory::new();
    let store = directory.open();
    let account = store
        .save_opencode_account(&user("qwen-account"), grant("qwen"), || true)
        .unwrap();
    let mut configuration = config("qwen-org");
    configuration["config"]["provider"]["opencode-go"]["models"]["qwen3.8-flash"] = json!({
        "tool_call":true,"provider":{"api":"https://opencode.ai/inference/go/anthropic/v1","npm":"@ai-sdk/anthropic"}
    });
    store
        .connect_opencode_model(
            &account,
            &OpenCodeOrganization {
                id: "qwen-org".into(),
                name: "Qwen org".into(),
            },
            &configuration,
            "go-qwen".into(),
            "Qwen".into(),
            Some("qwen3.8-flash"),
        )
        .unwrap();
    let snapshot = store.snapshot("device-qwen").unwrap();
    assert!(crate::valid_opencode_projection(&snapshot));
    let mut forged = snapshot.clone();
    forged.providers[0].config.protocol =
        winwincode_api::generated::DeviceProviderProtocol::OpenaiChatCompletions;
    assert!(!crate::valid_opencode_projection(&forged));
    let session = ProductSessionId("psn_000000000000000000000000Q8".into());
    let binding = store
        .bind_opencode_session(&session, "go-qwen")
        .unwrap()
        .unwrap();
    let headers = store.opencode_model_headers(&binding).unwrap();
    assert_eq!(headers["x-opencode-org-id"], "qwen-org");
    assert_eq!(
        store.opencode_access(&account, || true).unwrap().expose(),
        b"access-qwen"
    );
    drop(store);
    let reopened = directory.open();
    assert_eq!(
        reopened
            .bind_opencode_session(&session, "go-qwen")
            .unwrap()
            .unwrap(),
        binding
    );
    assert_eq!(reopened.opencode_model_headers(&binding).unwrap(), headers);
    assert_eq!(reopened.snapshot("device-qwen").unwrap(), snapshot);
}

#[test]
fn slow_refresh_does_not_hold_sqlite_transaction_or_block_another_account() {
    let directory = Directory::new();
    let store = directory.open();
    let a = store
        .save_opencode_account(&user("user-a"), grant("a"), || true)
        .unwrap();
    let b = store
        .save_opencode_account(&user("user-b"), grant("b"), || true)
        .unwrap();
    expire(&store, &a);
    let barrier = Arc::new(Barrier::new(2));
    let arrived = Arc::clone(&barrier);
    let path = directory.0.clone();
    let other_a = a.clone();
    let worker = thread::spawn(move || {
        let store = DeviceProviderStore::open(&path).unwrap();
        store
            .opencode_access_with(&other_a, &|| true, |old| {
                assert_eq!(old.expose(), b"refresh-a");
                assert!(store.connection.is_autocommit());
                arrived.wait();
                thread::sleep(Duration::from_millis(100));
                Ok(grant("a-new"))
            })
            .unwrap()
    });
    barrier.wait();
    assert_eq!(
        store.opencode_access(&b, || true).unwrap().expose(),
        b"access-b"
    );
    assert_eq!(worker.join().unwrap().expose(), b"access-a-new");
    let count = AtomicUsize::new(0);
    assert_eq!(
        store
            .opencode_access_with(&a, &|| true, |_| {
                count.fetch_add(1, Ordering::SeqCst);
                Ok(grant("unexpected"))
            })
            .unwrap()
            .expose(),
        b"access-a-new"
    );
    assert_eq!(count.load(Ordering::SeqCst), 0);
}

#[test]
fn uncertain_refresh_and_crash_before_save_require_reauthorization_without_replay() {
    let directory = Directory::new();
    let store = directory.open();
    let account = store
        .save_opencode_account(&user("user-a"), grant("old"), || true)
        .unwrap();
    expire(&store, &account);
    let count = AtomicUsize::new(0);
    let first = store.opencode_access_with(&account, &|| true, |_| {
        count.fetch_add(1, Ordering::SeqCst);
        Err(OpenCodeAuthError::Transport)
    });
    assert!(matches!(
        first,
        Err(OpenCodeCredentialError::ReauthorizationRequired)
    ));
    let second = store.opencode_access_with(&account, &|| true, |_| {
        count.fetch_add(1, Ordering::SeqCst);
        Ok(grant("never"))
    });
    assert!(matches!(
        second,
        Err(OpenCodeCredentialError::ReauthorizationRequired)
    ));
    assert_eq!(count.load(Ordering::SeqCst), 1);
    store
        .save_opencode_account(&user("user-a"), grant("reauthorized"), || true)
        .unwrap();
    expire(&store, &account);
    store.connection.execute_batch("CREATE TRIGGER fail_token_save BEFORE UPDATE OF access_token ON opencode_accounts
        WHEN OLD.state='refresh_in_flight' AND NEW.state='authorized' BEGIN SELECT RAISE(ABORT,'fixture save crash'); END;").unwrap();
    assert!(
        store
            .opencode_access_with(&account, &|| true, |_| Ok(grant("remote-rotated")))
            .is_err()
    );
    store
        .connection
        .execute_batch("DROP TRIGGER fail_token_save")
        .unwrap();
    drop(store);
    let store = directory.open();
    assert!(matches!(
        store.opencode_access_with(&account, &|| true, |_| panic!("old refresh token replayed")),
        Err(OpenCodeCredentialError::ReauthorizationRequired)
    ));
    let projection = store.opencode_accounts().unwrap();
    assert_eq!(
        projection[0].state,
        DeviceProviderOpenCodeAccountState::ReauthorizationRequired
    );
}

#[test]
fn binding_survives_restart_and_detects_account_or_organization_replacement() {
    let directory = Directory::new();
    let store = directory.open();
    let a = store
        .save_opencode_account(&user("user-a"), grant("a"), || true)
        .unwrap();
    let b = store
        .save_opencode_account(&user("user-b"), grant("b"), || true)
        .unwrap();
    connect(&store, &a, "go-a", "org-a");
    connect(&store, &b, "go-b", "org-b");
    let session = ProductSessionId("psn_00000000000000000000000001".into());
    let binding = store
        .bind_opencode_session(&session, "go-a")
        .unwrap()
        .unwrap();
    let headers = store.opencode_model_headers(&binding).unwrap();
    assert_eq!(headers["x-opencode-org-id"], "org-a");
    let other = store
        .bind_opencode_session(&session, "go-b")
        .unwrap()
        .unwrap();
    assert_ne!(binding.conversation_id, other.conversation_id);
    drop(store);
    let store = directory.open();
    assert_eq!(
        store
            .bind_opencode_session(&session, "go-a")
            .unwrap()
            .unwrap(),
        binding
    );
    store.connection.execute("UPDATE opencode_connections SET account_ref=?1,organization_id='org-b' WHERE provider_id='go-a'",[&b.0]).unwrap();
    assert!(matches!(
        store.bind_opencode_session(&session, "go-a"),
        Err(OpenCodeCredentialError::ConnectionChanged)
    ));
    store
        .connection
        .execute(
            "DELETE FROM opencode_connections WHERE provider_id='go-a'",
            [],
        )
        .unwrap();
    assert!(matches!(
        store.bind_opencode_session(&session, "go-a"),
        Err(OpenCodeCredentialError::ConnectionChanged)
    ));
}

#[test]
fn cancellation_and_logout_block_new_credential_operations() {
    let directory = Directory::new();
    let store = directory.open();
    let account = store
        .save_opencode_account(&user("user-a"), grant("a"), || true)
        .unwrap();
    connect(&store, &account, "go-a", "org-a");
    assert!(matches!(
        store.opencode_access(&account, || false),
        Err(OpenCodeCredentialError::Cancelled)
    ));
    store.logout_opencode(&account).unwrap();
    assert!(matches!(
        store.opencode_access(&account, || true),
        Err(OpenCodeCredentialError::ReauthorizationRequired)
    ));
    assert!(!store.snapshot("device").unwrap().providers[0].credential_configured);
    assert_eq!(
        store.opencode_accounts().unwrap()[0].state,
        DeviceProviderOpenCodeAccountState::LoggedOut
    );
}

#[test]
fn account_refresh_lock_is_shared_across_worker_processes() {
    const CHILD: &str = "WWC_OPENCODE_REFRESH_TEST_DIRECTORY";
    if let Some(directory) = std::env::var_os(CHILD) {
        let store = DeviceProviderStore::open(Path::new(&directory)).unwrap();
        let account = account_id("user-a");
        let access = store
            .opencode_access_with(&account, &|| true, |_| {
                assert!(store.connection.is_autocommit());
                store
                    .connection
                    .execute("UPDATE refresh_counter SET calls=calls+1", [])
                    .unwrap();
                thread::sleep(Duration::from_millis(100));
                Ok(grant("one-refresh"))
            })
            .unwrap();
        assert_eq!(access.expose(), b"access-one-refresh");
        return;
    }
    let directory = Directory::new();
    let store = directory.open();
    let account = store
        .save_opencode_account(&user("user-a"), grant("old"), || true)
        .unwrap();
    expire(&store, &account);
    store.connection.execute_batch("CREATE TABLE refresh_counter(calls INTEGER NOT NULL);INSERT INTO refresh_counter VALUES(0);").unwrap();
    let spawn = || {
        Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "opencode_store::tests::account_refresh_lock_is_shared_across_worker_processes",
                "--nocapture",
            ])
            .env(CHILD, &directory.0)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap()
    };
    let first = spawn();
    let second = spawn();
    for child in [first, second] {
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let calls: i64 = store
        .connection
        .query_row("SELECT calls FROM refresh_counter", [], |row| row.get(0))
        .unwrap();
    assert_eq!(calls, 1);
    assert_eq!(store.opencode_accounts().unwrap()[0].credential_version, 2);
}
