// SPDX-License-Identifier: Apache-2.0

//! Credential rotation must survive ordinary configuration saves without reviving replaced routes.

use super::*;
use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc::{self, Sender},
    },
    thread::{self, JoinHandle},
};

type Resolution =
    Result<(DeviceProviderConfig, ResolvedSecret, Option<String>), DeviceProviderError>;

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> (Self, DeviceProviderStore, DeviceProviderConfig) {
        static SEQUENCE: AtomicUsize = AtomicUsize::new(0);
        let directory = std::env::temp_dir().join(format!(
            "wwc-refresh-interleaving-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let fixture = Self(directory);
        let store = DeviceProviderStore::open(&fixture.0).unwrap();
        let config = DeviceProviderConfig {
            provider_id: "chatgpt-fixture".into(),
            display_name: "Original subscription".into(),
            endpoint: crate::chatgpt_oauth::ENDPOINT.into(),
            protocol: DeviceProviderProtocol::ChatgptPlan,
            responses_structured_output: None,
            model_ids: vec!["original-model".into()],
            enabled: true,
        };
        store
            .connection
            .execute(
                "INSERT INTO providers VALUES (?1, ?2, ?3)",
                params![
                    config.provider_id,
                    serde_json::to_string(&config).unwrap(),
                    serde_json::to_vec(&record(false)).unwrap()
                ],
            )
            .unwrap();
        (fixture, store, config)
    }

    fn paused_refresh(&self, provider_id: &str) -> (JoinHandle<Resolution>, Sender<()>) {
        let store = DeviceProviderStore::open(&self.0).unwrap();
        store
            .connection
            .busy_timeout(Duration::from_millis(100))
            .unwrap();
        let provider_id = provider_id.to_owned();
        let (ready, started) = mpsc::channel();
        let (resume, released) = mpsc::channel();
        let refresh = thread::spawn(move || {
            store.resolve_chatgpt_connection(&provider_id, |credentials| {
                *credentials = serde_json::from_value(record(true)).unwrap();
                ready.send(()).unwrap();
                released
                    .recv_timeout(Duration::from_secs(5))
                    .map_err(|_| DeviceProviderError)?;
                Ok(())
            })
        });
        started.recv_timeout(Duration::from_secs(5)).unwrap();
        (refresh, resume)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn record(rotated: bool) -> serde_json::Value {
    serde_json::json!({
        "client_id": "oaiapp_fixture", "subject": "fixture-subject",
        "access_token": if rotated { "fixture-access-r2" } else { "fixture-access-r1" },
        "refresh_token": if rotated { "fixture-refresh-r2" } else { "fixture-refresh-r1" },
        "id_token": "fixture-id", "scope": "resource.invoke chatgpt.tokens.use.direct",
        "expires_at": if rotated { u64::MAX } else { 0 }
    })
}

fn envelope(
    store: &DeviceProviderStore,
    operation: &str,
    config: &DeviceProviderConfig,
    api_key: Option<&str>,
) -> DeviceConfigurationEnvelope {
    let snapshot = store.snapshot("device").unwrap();
    let ephemeral = new_secret_key().unwrap();
    let public =
        PublicKey::from_sec1_bytes(&STANDARD.decode(snapshot.encryption_public_key).unwrap())
            .unwrap();
    let shared = diffie_hellman(ephemeral.to_nonzero_scalar(), public.as_affine());
    let request_id = format!("refresh_fixture_{}", snapshot.revision);
    let aad = format!("{CONTEXT}\ndevice\n{request_id}\n{}", snapshot.revision);
    let mut key = [0; 32];
    Hkdf::<Sha256>::new(Some(CONTEXT.as_bytes()), shared.raw_secret_bytes())
        .expand(aad.as_bytes(), &mut key)
        .unwrap();
    let cipher = Aes256Gcm::new_from_slice(&key).unwrap();
    key.fill(0);
    let mut nonce = [0; 12];
    getrandom::fill(&mut nonce).unwrap();
    let plaintext = serde_json::to_vec(&serde_json::json!({
        "operation": operation, "config": config, "apiKey": api_key
    }))
    .unwrap();
    let ciphertext = cipher
        .encrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: &plaintext,
                aad: aad.as_bytes(),
            },
        )
        .unwrap();
    DeviceConfigurationEnvelope {
        ciphertext: STANDARD.encode(ciphertext),
        client_node_id: "device".into(),
        expected_revision: snapshot.revision,
        nonce: STANDARD.encode(nonce),
        public_key: STANDARD.encode(ephemeral.public_key().to_encoded_point(false).as_bytes()),
        request_id,
    }
}

fn apply(
    store: &mut DeviceProviderStore,
    operation: &str,
    config: &DeviceProviderConfig,
    api_key: Option<&str>,
) -> DeviceProviderOutcome {
    let envelope = envelope(store, operation, config, api_key);
    store.apply("device", &envelope).unwrap().outcome
}

fn preserves_rotation(change: impl FnOnce(&mut DeviceProviderConfig)) {
    let (fixture, mut store, mut config) = Fixture::new();
    let previous = ResolvedSecret::from_bytes(
        store
            .connection
            .query_row(
                "SELECT secret FROM providers WHERE provider_id=?1",
                [&config.provider_id],
                |row| row.get(0),
            )
            .unwrap(),
    )
    .unwrap();
    let (refresh, resume) = fixture.paused_refresh(&config.provider_id);
    change(&mut config);
    let saved = apply(&mut store, "save", &config, None);
    let after_save = ResolvedSecret::from_bytes(
        store
            .connection
            .query_row(
                "SELECT secret FROM providers WHERE provider_id=?1",
                [&config.provider_id],
                |row| row.get(0),
            )
            .unwrap(),
    )
    .unwrap();
    let same_credential = serde_json::from_slice::<serde_json::Value>(previous.expose()).unwrap()
        == serde_json::from_slice::<serde_json::Value>(after_save.expose()).unwrap();
    let same_bytes = previous.expose() == after_save.expose();
    resume.send(()).unwrap();
    let result = refresh.join().unwrap();
    assert_eq!(saved, DeviceProviderOutcome::Saved);
    assert!(
        same_credential,
        "configuration save changed credential semantics"
    );
    assert!(
        same_bytes,
        "configuration save re-encoded an unchanged credential before rotation CAS"
    );
    let (resolved, secret, _) = result.expect("configuration saves preserve successful rotation");
    assert_eq!(resolved, config, "resolve returns the latest configuration");
    assert!(secret.expose() == b"fixture-access-r2");
    let persisted =
        serde_json::to_value(store.chatgpt_credentials(&config.provider_id).unwrap()).unwrap();
    assert!(persisted["refresh_token"] == "fixture-refresh-r2");
    let calls = std::cell::Cell::new(0);
    let (next, _, _) = store
        .resolve_chatgpt_connection(&config.provider_id, |_| {
            calls.set(calls.get() + 1);
            Err(DeviceProviderError)
        })
        .expect("the next resolve reuses the rotated credential");
    assert_eq!(calls.get(), 0);
    assert_eq!(next, config);
}

#[test]
fn refresh_rotation_survives_display_name_save() {
    preserves_rotation(|config| config.display_name = "Renamed subscription".into());
}

#[test]
fn refresh_rotation_returns_latest_model_selection() {
    preserves_rotation(|config| config.model_ids = vec!["replacement-model".into()]);
}

#[test]
fn refresh_rotation_returns_disabled_configuration() {
    preserves_rotation(|config| config.enabled = false);
}

#[test]
fn refresh_rotation_does_not_resurrect_deleted_provider() {
    let (fixture, mut store, config) = Fixture::new();
    let (refresh, resume) = fixture.paused_refresh(&config.provider_id);
    let outcome = apply(&mut store, "delete", &config, None);
    resume.send(()).unwrap();
    assert_eq!(outcome, DeviceProviderOutcome::Deleted);
    assert!(refresh.join().unwrap().is_err());
    assert!(store.snapshot("device").unwrap().providers.is_empty());
}

#[test]
fn refresh_rotation_does_not_overwrite_protocol_replacement() {
    let (fixture, mut store, mut config) = Fixture::new();
    let (refresh, resume) = fixture.paused_refresh(&config.provider_id);
    config.protocol = DeviceProviderProtocol::Canonical;
    config.endpoint = "https://example.com/responses".into();
    let outcome = apply(&mut store, "save", &config, Some("fixture-replacement-key"));
    resume.send(()).unwrap();
    assert_eq!(outcome, DeviceProviderOutcome::Saved);
    assert!(refresh.join().unwrap().is_err());
    let (resolved, secret) = store.resolve(&config.provider_id).unwrap();
    assert_eq!(resolved, config);
    assert!(secret.expose() == b"fixture-replacement-key");
}

#[test]
fn refresh_rotation_does_not_overwrite_reauthorization() {
    let (fixture, store, config) = Fixture::new();
    let (refresh, resume) = fixture.paused_refresh(&config.provider_id);
    let mut replacement = record(true);
    replacement["subject"] = serde_json::json!("replacement-subject");
    replacement["access_token"] = serde_json::json!("fixture-new-authorization");
    let mut receipt = DeviceProviderReceipt {
        request_id: "refresh_fixture_authorize".into(),
        revision: store.revision().unwrap(),
        outcome: DeviceProviderOutcome::Interrupted,
    };
    store
        .finish_authorization(
            Mutation {
                operation: "authorize".into(),
                config: config.clone(),
                api_key: None,
                custom_headers: None,
            },
            &mut receipt,
            Ok((
                serde_json::from_value(replacement).unwrap(),
                config.model_ids.clone(),
            )),
        )
        .unwrap();
    resume.send(()).unwrap();
    assert_eq!(receipt.outcome, DeviceProviderOutcome::Saved);
    assert!(refresh.join().unwrap().is_err());
    assert!(store.resolve(&config.provider_id).unwrap().1.expose() == b"fixture-new-authorization");
}

#[test]
fn incompatible_save_without_key_never_waits_for_a_credential_refresh() {
    let (fixture, mut store, mut config) = Fixture::new();
    // If the old save path reaches refresh after the lock is released, fail locally.
    // This keeps the regression entirely offline even on the broken implementation.
    let mut expired = record(false);
    expired["refresh_token"] = serde_json::json!("\n");
    store
        .connection
        .execute(
            "UPDATE providers SET secret=?1 WHERE provider_id=?2",
            params![serde_json::to_vec(&expired).unwrap(), config.provider_id],
        )
        .unwrap();
    let (refresh, resume) = fixture.paused_refresh(&config.provider_id);
    config.protocol = DeviceProviderProtocol::Canonical;
    config.endpoint = "https://example.com/responses".into();
    let envelope = envelope(&store, "save", &config, None);
    let (done, completed) = mpsc::channel();
    let (started, running) = mpsc::channel();
    let save = thread::spawn(move || {
        started.send(()).unwrap();
        let result = store
            .apply("device", &envelope)
            .map(|receipt| receipt.outcome);
        done.send(result).unwrap();
    });
    running.recv_timeout(Duration::from_secs(5)).unwrap();
    let immediate = completed.recv_timeout(Duration::from_secs(2));
    let rejected_while_refresh_is_paused = immediate.is_ok();
    resume.send(()).unwrap();
    let refreshed = refresh.join().unwrap();
    let outcome =
        immediate.unwrap_or_else(|_| completed.recv_timeout(Duration::from_secs(5)).unwrap());
    save.join().unwrap();
    assert!(
        rejected_while_refresh_is_paused,
        "incompatible key reuse must reject before acquiring the credential lock"
    );
    assert_eq!(outcome.unwrap(), DeviceProviderOutcome::InvalidRequest);
    assert!(
        refreshed.is_ok(),
        "rejected saves cannot block rotation persistence"
    );
}
