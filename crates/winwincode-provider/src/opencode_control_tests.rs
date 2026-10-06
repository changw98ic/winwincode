// SPDX-License-Identifier: Apache-2.0

use super::*;
use crate::opencode_auth::{OpenCodeTokenGrant, OpenCodeUser};
use aes_gcm::{
    Aes256Gcm, Nonce,
    aead::{Aead as _, KeyInit as _, Payload},
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use hkdf::Hkdf;
use p256::{PublicKey, SecretKey, ecdh::diffie_hellman, elliptic_curve::sec1::ToEncodedPoint as _};
use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

struct Fixture(PathBuf);
static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);
impl Fixture {
    fn new() -> Self {
        Self(std::env::temp_dir().join(format!(
                "wwc-opencode-controls-{}-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos(),
                NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
            )))
    }
    fn open(&self) -> DeviceProviderStore {
        DeviceProviderStore::open(&self.0).unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn login() -> DeviceProviderOpenCodeLogin {
    DeviceProviderOpenCodeLogin {
        login_id: OpenCodeLoginId("ocl_00000000000000000000000001".into()),
        state: LoginState::Pending,
        verification_uri: Some(
            "https://opencode.ai/console/device?user_code=SAFE-CODE&client_id=opencode-cli".into(),
        ),
        user_code: Some("SAFE-CODE".into()),
        expires_at_ms: now_ms().unwrap() + 60_000,
        poll_after_ms: 0,
        account_ref: None,
        organizations: Vec::new(),
    }
}
fn token() -> OpenCodeTokenGrant {
    OpenCodeTokenGrant {
        access_token: ResolvedSecret::from_bytes(b"access-secret-fixture".to_vec()).unwrap(),
        refresh_token: ResolvedSecret::from_bytes(b"refresh-secret-fixture".to_vec()).unwrap(),
        expires_in: Duration::from_hours(1),
    }
}
fn envelope(
    store: &DeviceProviderStore,
    id: &str,
    command: &serde_json::Value,
) -> DeviceConfigurationEnvelope {
    let snapshot = store.snapshot("control-device").unwrap();
    let local = SecretKey::from_slice(&[2; 32]).unwrap();
    let remote =
        PublicKey::from_sec1_bytes(&STANDARD.decode(&snapshot.encryption_public_key).unwrap())
            .unwrap();
    let shared = diffie_hellman(local.to_nonzero_scalar(), remote.as_affine());
    let aad = format!("{CONTEXT}\ncontrol-device\n{id}\n{}", snapshot.revision);
    let mut key = [0; 32];
    Hkdf::<Sha256>::new(Some(CONTEXT.as_bytes()), shared.raw_secret_bytes())
        .expand(aad.as_bytes(), &mut key)
        .unwrap();
    let mut nonce = [0; 12];
    getrandom::fill(&mut nonce).unwrap();
    let ciphertext = Aes256Gcm::new_from_slice(&key)
        .unwrap()
        .encrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: &serde_json::to_vec(command).unwrap(),
                aad: aad.as_bytes(),
            },
        )
        .unwrap();
    DeviceConfigurationEnvelope {
        client_node_id: "control-device".into(),
        request_id: id.into(),
        expected_revision: snapshot.revision,
        public_key: STANDARD.encode(local.public_key().to_encoded_point(false).as_bytes()),
        nonce: STANDARD.encode(nonce),
        ciphertext: STANDARD.encode(ciphertext),
    }
}

#[test]
fn polls_obey_schedule_slow_down_and_expiry_and_clear_private_codes() {
    let fixture = Fixture::new();
    let store = fixture.open();
    let mut login = login();
    login.poll_after_ms = now_ms().unwrap() + 20_000;
    store
        .write_login(&login, b"device-secret-fixture", 5000, false)
        .unwrap();
    store
        .poll_opencode_login_with(
            &login.login_id,
            |_| panic!("early poll sent HTTP"),
            |_| panic!("early poll fetched identity"),
        )
        .unwrap();
    login.poll_after_ms = 0;
    store
        .write_login(&login, b"device-secret-fixture", 5000, false)
        .unwrap();
    store
        .poll_opencode_login_with(
            &login.login_id,
            |private| {
                assert!(store.connection.is_autocommit());
                assert_eq!(private.expose(), b"device-secret-fixture");
                Ok(OpenCodePollResult::SlowDown)
            },
            |_| panic!("pending poll fetched identity"),
        )
        .unwrap();
    let (mut projection, private, interval, in_flight) = store.read_login(&login.login_id).unwrap();
    assert_eq!(interval, 10_000);
    assert!(!in_flight);
    assert!(private.is_some());
    assert!(projection.poll_after_ms >= now_ms().unwrap() + 9900);
    projection.expires_at_ms = 0;
    store
        .write_login(&projection, b"device-secret-fixture", interval, false)
        .unwrap();
    store
        .poll_opencode_login_with(
            &login.login_id,
            |_| panic!("expired poll sent HTTP"),
            |_| panic!("expired poll fetched identity"),
        )
        .unwrap();
    let (expired, private, _, _) = store.read_login(&login.login_id).unwrap();
    assert_eq!(expired.state, LoginState::Expired);
    assert!(private.is_none());
    assert!(expired.user_code.is_none());
    assert!(expired.verification_uri.is_none());
}

#[test]
fn authorized_poll_saves_verified_identity_and_never_repolls_terminal_grant() {
    let fixture = Fixture::new();
    let store = fixture.open();
    let login = login();
    store
        .write_login(&login, b"device-secret-fixture", 5000, false)
        .unwrap();
    store
        .poll_opencode_login_with(
            &login.login_id,
            |_| Ok(OpenCodePollResult::Authorized(token())),
            |access| {
                assert!(store.connection.is_autocommit());
                assert_eq!(access.expose(), b"access-secret-fixture");
                Ok((
                    OpenCodeUser {
                        id: "actual-account-a".into(),
                        email: "actual-a@example.test".into(),
                    },
                    vec![
                        OpenCodeOrganization {
                            id: "org-a".into(),
                            name: "A".into(),
                        },
                        OpenCodeOrganization {
                            id: "org-b".into(),
                            name: "B".into(),
                        },
                    ],
                ))
            },
        )
        .unwrap();
    let (authorized, private, _, _) = store.read_login(&login.login_id).unwrap();
    assert_eq!(authorized.state, LoginState::Authorized);
    assert!(private.is_none());
    assert_eq!(authorized.organizations.len(), 2);
    store
        .poll_opencode_login_with(
            &login.login_id,
            |_| panic!("terminal grant was resubmitted"),
            |_| panic!("terminal grant was resubmitted"),
        )
        .unwrap();
    let snapshot = store.snapshot("control-device").unwrap();
    let account = &snapshot.open_code_accounts.as_ref().unwrap()[0];
    assert_eq!(account.email, "actual-a@example.test");
    assert_eq!(account.account_ref, authorized.account_ref.unwrap());
    let public = serde_json::to_string(&snapshot).unwrap();
    for secret in [
        "access-secret-fixture",
        "refresh-secret-fixture",
        "device-secret-fixture",
    ] {
        assert!(!public.contains(secret));
    }
}

#[test]
fn uncertain_poll_is_not_replayed_and_denial_and_transport_errors_are_terminal() {
    let fixture = Fixture::new();
    let store = fixture.open();
    let login = login();
    store
        .write_login(&login, b"device-secret-fixture", 5000, true)
        .unwrap();
    store
        .poll_opencode_login_with(
            &login.login_id,
            |_| panic!("uncertain poll was replayed"),
            |_| panic!("uncertain poll fetched identity"),
        )
        .unwrap();
    assert_eq!(
        store.read_login(&login.login_id).unwrap().0.state,
        LoginState::Failed
    );
    for (error, expected) in [
        (OpenCodeAuthError::Denied, LoginState::Denied),
        (OpenCodeAuthError::Transport, LoginState::Failed),
        (OpenCodeAuthError::Cancelled, LoginState::Cancelled),
    ] {
        store
            .write_login(&login, b"device-secret-fixture", 5000, false)
            .unwrap();
        store
            .poll_opencode_login_with(
                &login.login_id,
                |_| Err(error),
                |_| panic!("rejected poll fetched identity"),
            )
            .unwrap();
        let (ended, private, _, _) = store.read_login(&login.login_id).unwrap();
        assert_eq!(ended.state, expected);
        assert!(private.is_none());
    }
}

#[test]
fn encrypted_controls_validate_shape_and_return_exact_replay_receipts() {
    let fixture = Fixture::new();
    let mut store = fixture.open();
    let login = login();
    store
        .write_login(&login, b"device-secret-fixture", 5000, false)
        .unwrap();
    let invalid = envelope(
        &store,
        "invalid-control-1",
        &serde_json::json!({"operation":"poll_opencode_login"}),
    );
    assert_eq!(
        store.apply("control-device", &invalid).unwrap().outcome,
        DeviceProviderOutcome::InvalidRequest
    );
    let invalid = envelope(
        &store,
        "invalid-model-selector",
        &serde_json::json!({"operation":"cancel_opencode_login","loginId":login.login_id.0,"modelId":"qwen3.8-flash"}),
    );
    assert_eq!(
        store.apply("control-device", &invalid).unwrap().outcome,
        DeviceProviderOutcome::InvalidRequest
    );
    let invalid = envelope(
        &store,
        "invalid-control-2",
        &serde_json::json!({"operation":"begin_opencode_login","accountRef":"oca_00000000000000000000000001"}),
    );
    assert_eq!(
        store.apply("control-device", &invalid).unwrap().outcome,
        DeviceProviderOutcome::InvalidRequest
    );
    let command =
        &serde_json::json!({"operation":"cancel_opencode_login","loginId":login.login_id.0});
    let cancel = envelope(&store, "cancel-control-1", command);
    let receipt = store.apply("control-device", &cancel).unwrap();
    assert_eq!(receipt.outcome, DeviceProviderOutcome::Saved);
    assert_eq!(store.apply("control-device", &cancel).unwrap(), receipt);
    let (cancelled, private, _, _) = store.read_login(&login.login_id).unwrap();
    assert_eq!(cancelled.state, LoginState::Cancelled);
    assert!(private.is_none());
    let changed = envelope(&store, "cancel-control-1", command);
    assert!(store.apply("control-device", &changed).is_err());
}

#[test]
fn projection_gate_rejects_a_token_in_upstream_account_metadata() {
    let fixture = Fixture::new();
    let store = fixture.open();
    store
        .save_opencode_account(
            &OpenCodeUser {
                id: "actual-a".into(),
                email: "access-secret-fixture".into(),
            },
            token(),
            || true,
        )
        .unwrap();
    assert!(store.snapshot("control-device").is_err());
}

#[test]
fn encrypted_oauth_edits_reject_route_changes_and_deleted_identity_reuse() {
    let fixture = Fixture::new();
    let mut store = fixture.open();
    let account = store
        .save_opencode_account(
            &OpenCodeUser {
                id: "actual-a".into(),
                email: "a@example.test".into(),
            },
            token(),
            || true,
        )
        .unwrap();
    let configuration = serde_json::json!({"config":{"provider":{"opencode-go":{
        "api":"https://opencode.ai/inference/go/openai/v1", "npm":"@ai-sdk/openai-compatible",
        "options":{"headers":{"x-opencode-org-id":"org-a"}},"models":{"glm-5.3-flash":{"tool_call":true}}}}}});
    let org = OpenCodeOrganization {
        id: "org-a".into(),
        name: "A".into(),
    };
    store
        .connect_opencode(&account, &org, &configuration, "go-a".into(), "A".into())
        .unwrap();
    let original = store.snapshot("control-device").unwrap().providers[0]
        .config
        .clone();
    let mut changed = original.clone();
    changed.endpoint = "https://evil.test/v1/chat/completions".into();
    for operation in ["save", "test"] {
        let command = envelope(
            &store,
            &format!("reject-{operation}"),
            &serde_json::json!({"operation":operation,"config":changed}),
        );
        let outcome = store.apply("control-device", &command).unwrap().outcome;
        assert!(matches!(
            outcome,
            DeviceProviderOutcome::InvalidRequest | DeviceProviderOutcome::ProviderUnavailable
        ));
    }
    changed = original.clone();
    changed.display_name = "Renamed".into();
    changed.enabled = false;
    let command = envelope(
        &store,
        "rename-connection",
        &serde_json::json!({"operation":"save","config":changed}),
    );
    assert_eq!(
        store.apply("control-device", &command).unwrap().outcome,
        DeviceProviderOutcome::Saved
    );
    let command = envelope(
        &store,
        "delete-connection",
        &serde_json::json!({"operation":"delete","config":original}),
    );
    assert_eq!(
        store.apply("control-device", &command).unwrap().outcome,
        DeviceProviderOutcome::Deleted
    );
    assert!(
        store
            .snapshot("control-device")
            .unwrap()
            .providers
            .is_empty()
    );
    assert!(
        store
            .connect_opencode(
                &account,
                &org,
                &configuration,
                "go-a".into(),
                "Reused".into()
            )
            .is_err()
    );
    let command = envelope(
        &store,
        "static-reuse",
        &serde_json::json!({"operation":"save","config":original,"apiKey":"replacement-secret"}),
    );
    assert_eq!(
        store.apply("control-device", &command).unwrap().outcome,
        DeviceProviderOutcome::InvalidRequest
    );
}
