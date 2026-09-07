// SPDX-License-Identifier: Apache-2.0

//! Day-use Provider onboarding coverage: plaintext non-persistence, the
//! test-before-route gate, rotation atomicity, and failure cleanup.
//!
//! Every flow below is driven through the public application surface only:
//! requests carry a preset id or custom endpoint plus the secret, and every
//! Credential reference id comes from a previous call's output. No test ever
//! hand-types an internal Credential ID.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use winwincode_api::generated::{
    Actor, CredentialReferenceListParameters, CredentialReferenceListQuery,
    CredentialReferenceListQueryQuery, OrganizationScope, OrganizationScopeKind, PageRequest,
    SchemaVersion, Scope, UserActor, UserActorKind,
};
use winwincode_control_plane::{
    CatalogAvailability, CredentialLeakErrorKind, CredentialLeakGate, CredentialOutputBoundary,
    CredentialReferenceResolution, CredentialReferenceService, LocalSecretStoreAdapter,
    ModelCapability, ModelCapabilitySource, ModelSettingsService, ModelSettingsTarget,
    ModelToolSupport, ProviderCatalogService, ResolvedSecret, SecretStoreError,
    SecretStoreErrorKind, SecretStorePort,
};
use winwincode_domain::{CredentialReferenceId, OrganizationId, RequestId};
use winwincode_server::{
    ConnectionProbe, ConnectionTestReport, CreateCredentialRequest, CredentialReferenceOnboarded,
    EstablishRouteRequest, OnboardProviderRequest, OnboardingSecretStore, ProbeOutcome,
    ProviderOnboardingErrorKind, ProviderOnboardingService, RotateCredentialRequest,
    TestConnectionRequest,
};
use winwincode_storage::{CommitReceipt, ProductStateStorage, SqliteStorage, StoredState};

/// A fixture shaped like a real Provider API key: the leak gate's recognized
/// encoding detector flags it on sight, so every non-persistence assertion
/// below is meaningful.
const USER_SECRET: &str = "sk-onboarding-fixture-0123456789abcdef";
const ROTATED_SECRET: &str = "sk-rotated-fixture-0123456789abcdef";
const OWNER: &str = "usr_00000000000000000000000001";
const ORGANIZATION: &str = "org_00000000000000000000000001";
const PROVIDER: &str = "deepseek";
const ROTATED_MARKER: &[u8] = b"\"rotated\"";
const ROTATE_ACTION: &[u8] = b"credential.reference.rotate";

fn actor() -> Actor {
    Actor::UserActor(UserActor {
        kind: UserActorKind::User,
        id: winwincode_domain::UserId(OWNER.to_owned()),
    })
}

fn organization_scope() -> OrganizationScope {
    OrganizationScope {
        kind: OrganizationScopeKind::Organization,
        organization_id: OrganizationId(ORGANIZATION.to_owned()),
    }
}

fn scope() -> Scope {
    Scope::OrganizationScope(organization_scope())
}

fn secret(text: &str) -> ResolvedSecret {
    ResolvedSecret::from_bytes(text.as_bytes().to_vec()).expect("fixture secret is non-empty")
}

fn confirmed_deepseek_models() -> Vec<ModelCapability> {
    vec![
        ModelCapability {
            model_id: "deepseek-chat".to_owned(),
            display_name: "DeepSeek Chat".to_owned(),
            context_window_tokens: 128_000,
            max_output_tokens: 8_000,
            tool_support: ModelToolSupport::Parallel,
            reasoning_efforts: vec![],
        },
        ModelCapability {
            model_id: "deepseek-reasoner".to_owned(),
            display_name: "DeepSeek Reasoner".to_owned(),
            context_window_tokens: 128_000,
            max_output_tokens: 8_000,
            tool_support: ModelToolSupport::Serial,
            reasoning_efforts: vec!["medium".to_owned()],
        },
    ]
}

/// Records every probe call so tests can assert which endpoint and which
/// stored credential bytes reached the provider-call boundary.
#[derive(Default)]
struct FakeProbe {
    outcome: Mutex<ProbeOutcome>,
    calls: Mutex<Vec<(String, String, Vec<u8>)>>,
}

impl FakeProbe {
    fn confirmed(models: Vec<ModelCapability>) -> Self {
        Self {
            outcome: Mutex::new(ProbeOutcome::connected(models)),
            calls: Mutex::new(Vec::new()),
        }
    }

    fn unreachable() -> Self {
        Self {
            outcome: Mutex::new(ProbeOutcome::unreachable()),
            calls: Mutex::new(Vec::new()),
        }
    }

    fn set_outcome(&self, outcome: ProbeOutcome) {
        *self.outcome.lock().expect("outcome lock") = outcome;
    }

    fn calls(&self) -> Vec<(String, String, Vec<u8>)> {
        self.calls.lock().expect("calls lock").clone()
    }
}

impl ConnectionProbe for FakeProbe {
    fn probe(
        &self,
        provider_id: &str,
        endpoint: &str,
        credential: &ResolvedSecret,
    ) -> ProbeOutcome {
        self.calls.lock().expect("calls lock").push((
            provider_id.to_owned(),
            endpoint.to_owned(),
            credential.expose().to_vec(),
        ));
        self.outcome.lock().expect("outcome lock").clone()
    }
}

/// Wraps the local adapter to inject a staging failure while every other
/// operation stays real.
struct FailStageStore {
    inner: LocalSecretStoreAdapter,
}

impl FailStageStore {
    fn new(inner: LocalSecretStoreAdapter) -> Self {
        Self { inner }
    }
}

impl SecretStorePort for FailStageStore {
    fn resolve(
        &self,
        reference: &CredentialReferenceResolution,
    ) -> Result<ResolvedSecret, SecretStoreError> {
        self.inner.resolve(reference)
    }
}

impl OnboardingSecretStore for FailStageStore {
    fn store(
        &self,
        reference: &CredentialReferenceResolution,
        secret: ResolvedSecret,
    ) -> Result<(), SecretStoreError> {
        self.inner.store(reference, secret).map(|_| ())
    }

    fn stage_rotation(
        &self,
        _current: &CredentialReferenceResolution,
        _secret: ResolvedSecret,
    ) -> Result<(), SecretStoreError> {
        Err(SecretStoreError::unavailable())
    }

    fn cleanup(&self, current: &CredentialReferenceResolution) -> Result<(), SecretStoreError> {
        self.inner.cleanup(current).map(|_| ())
    }

    fn delete(&self, reference: &CredentialReferenceResolution) -> Result<(), SecretStoreError> {
        self.inner.delete(reference).map(|_| ())
    }
}

/// A store whose first publication always fails; used to drive the
/// half-created-reference cleanup path.
#[derive(Default)]
struct RejectingStore;

impl SecretStorePort for RejectingStore {
    fn resolve(
        &self,
        _reference: &CredentialReferenceResolution,
    ) -> Result<ResolvedSecret, SecretStoreError> {
        Err(SecretStoreError::missing())
    }
}

impl OnboardingSecretStore for RejectingStore {
    fn store(
        &self,
        _reference: &CredentialReferenceResolution,
        _secret: ResolvedSecret,
    ) -> Result<(), SecretStoreError> {
        Err(SecretStoreError::unavailable())
    }

    fn stage_rotation(
        &self,
        _current: &CredentialReferenceResolution,
        _secret: ResolvedSecret,
    ) -> Result<(), SecretStoreError> {
        Err(SecretStoreError::unavailable())
    }

    fn cleanup(&self, _current: &CredentialReferenceResolution) -> Result<(), SecretStoreError> {
        Ok(())
    }

    fn delete(&self, _reference: &CredentialReferenceResolution) -> Result<(), SecretStoreError> {
        Ok(())
    }
}

struct Harness {
    data_directory: PathBuf,
    secret_directory: PathBuf,
    probe: FakeProbe,
}

fn harness(name: &str, probe: FakeProbe) -> Harness {
    let data_directory = std::env::temp_dir().join(format!(
        "winwincode-provider-onboarding-{name}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&data_directory);
    let secret_directory = data_directory.join("secrets");
    Harness {
        data_directory,
        secret_directory,
        probe,
    }
}

fn secret_files(secret_directory: &Path) -> Vec<(String, Vec<u8>)> {
    let mut files = Vec::new();
    let mut stack = vec![secret_directory.to_path_buf()];
    while let Some(directory) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if let Ok(bytes) = std::fs::read(&path) {
                files.push((entry.file_name().to_string_lossy().into_owned(), bytes));
            }
        }
    }
    files
}

fn stored_credential_state(storage: &mut SqliteStorage, id: &CredentialReferenceId) -> StoredState {
    storage
        .load_state(&format!("credential-reference:{}", id.0))
        .expect("credential reference state reads")
        .expect("credential reference state exists")
}

fn credential_receipt(
    storage: &mut SqliteStorage,
    id: &CredentialReferenceId,
    revision: u64,
) -> CommitReceipt {
    storage
        .load_receipt_for_stream_revision(&format!("credential-reference:{}", id.0), revision)
        .expect("credential reference receipt reads")
        .expect("credential reference receipt exists")
}

fn secret_fingerprint_gate(secret: &ResolvedSecret) -> CredentialLeakGate {
    let mut gate = CredentialLeakGate::new();
    gate.track_secret(secret);
    gate
}

fn assert_secret_free(gate: &CredentialLeakGate, bytes: &[u8]) {
    gate.inspect_bytes(CredentialOutputBoundary::Log, bytes)
        .expect("output is secret-free");
    assert!(
        !bytes
            .windows(USER_SECRET.len())
            .any(|window| window == USER_SECRET.as_bytes()),
        "output must not contain the plaintext secret"
    );
}

fn onboard(harness: &Harness, storage: &mut SqliteStorage) -> (CredentialReferenceOnboarded, u64) {
    let store = secret_store(harness);
    let mut service = ProviderOnboardingService::new(&mut *storage, &store, &harness.probe);
    let onboarded = service
        .onboard_provider(OnboardProviderRequest {
            actor: actor(),
            organization_scope: organization_scope(),
            provider_id: PROVIDER.to_owned(),
            custom_endpoint: None,
            secret: secret(USER_SECRET),
            default_model_id: Some("deepseek-chat".to_owned()),
        })
        .expect("onboarding reaches ready");
    let catalog_version = ProviderCatalogService::new(storage)
        .project(&scope())
        .expect("catalog projects")
        .catalog_version;
    (onboarded.credential_reference, catalog_version)
}

fn secret_store(harness: &Harness) -> LocalSecretStoreAdapter {
    LocalSecretStoreAdapter::open(&harness.secret_directory).expect("secret store opens")
}

fn assert_no_version_files(secret_directory: &Path) {
    let files = secret_files(secret_directory);
    assert!(
        files.iter().all(|(name, _)| !name.starts_with("version-")),
        "no immutable secret version file may remain, found: {files:?}"
    );
}

#[test]
fn end_to_end_onboarding_establishes_route_from_verified_capabilities() {
    let harness = harness("e2e", FakeProbe::confirmed(confirmed_deepseek_models()));
    let mut storage = SqliteStorage::open(&harness.data_directory).expect("storage opens");
    let store = secret_store(&harness);
    let mut service = ProviderOnboardingService::new(&mut storage, &store, &harness.probe);
    let onboarded = service
        .onboard_provider(OnboardProviderRequest {
            actor: actor(),
            organization_scope: organization_scope(),
            provider_id: PROVIDER.to_owned(),
            custom_endpoint: None,
            secret: secret(USER_SECRET),
            default_model_id: Some("deepseek-chat".to_owned()),
        })
        .expect("onboarding reaches ready");

    // The internal identity is derived: canonical crd_ id, never an input.
    let id = &onboarded.credential_reference.credential_reference_id;
    assert_eq!(id.0.len(), "crd_".len() + 26);
    assert!(id.0.starts_with("crd_"));
    assert_eq!(
        onboarded.credential_reference.endpoint, "https://api.deepseek.com/v1",
        "the preset endpoint was resolved for the user"
    );

    // The probe saw the preset endpoint and the exact stored secret bytes at
    // the provider-call boundary.
    let calls = harness.probe.calls();
    let [(provider_id, endpoint, credential)] = calls.as_slice() else {
        panic!("exactly one probe call expected, got {calls:?}");
    };
    assert_eq!(provider_id, PROVIDER);
    assert_eq!(endpoint, "https://api.deepseek.com/v1");
    assert_eq!(credential, USER_SECRET.as_bytes());

    // The Provider catalog descriptor pairs the preset with the derived
    // credential reference and only the probe-confirmed capabilities.
    let catalog = ProviderCatalogService::new(&mut storage)
        .project(&scope())
        .expect("catalog projects");
    let [provider] = catalog.providers.as_slice() else {
        panic!("exactly one provider expected");
    };
    assert_eq!(provider.provider_id, PROVIDER);
    assert_eq!(provider.display_name, "DeepSeek");
    assert_eq!(provider.adapter_kind, "openai-responses");
    assert_eq!(provider.availability, CatalogAvailability::Enabled);
    assert_eq!(&provider.credential_reference_id, id);
    let enabled_models = provider
        .models
        .iter()
        .filter(|model| model.availability == CatalogAvailability::Enabled)
        .count();
    assert_eq!(enabled_models, 2, "only probe-confirmed models registered");

    // The default route points at the verified provider/model/credential
    // triple without any hand-typed identity.
    let settings = ModelSettingsService::new(&mut storage)
        .project(&ModelSettingsTarget::Organization {
            scope: organization_scope(),
        })
        .expect("settings project");
    let route = settings.default_model_route.expect("default route set");
    assert_eq!(route.provider_id, PROVIDER);
    assert_eq!(route.model_id, "deepseek-chat");
    assert_eq!(&route.credential_reference_id, id);

    drop(storage);
    let _ = std::fs::remove_dir_all(&harness.data_directory);
}

#[test]
fn the_plaintext_secret_never_reaches_storage_events_audit_disk_or_responses() {
    let harness = harness(
        "non-persistence",
        FakeProbe::confirmed(confirmed_deepseek_models()),
    );
    let mut storage = SqliteStorage::open(&harness.data_directory).expect("storage opens");
    let store = secret_store(&harness);
    let mut service = ProviderOnboardingService::new(&mut storage, &store, &harness.probe);
    let onboarded = service
        .onboard_provider(OnboardProviderRequest {
            actor: actor(),
            organization_scope: organization_scope(),
            provider_id: PROVIDER.to_owned(),
            custom_endpoint: None,
            secret: secret(USER_SECRET),
            default_model_id: Some("deepseek-chat".to_owned()),
        })
        .expect("onboarding reaches ready");
    let id = onboarded
        .credential_reference
        .credential_reference_id
        .clone();

    // The fixture secret is detectable by the leak gate on sight, so every
    // assertion below is meaningful.
    let encoding_error = CredentialLeakGate::new()
        .inspect_bytes(CredentialOutputBoundary::Log, USER_SECRET.as_bytes())
        .expect_err("the fixture is a recognized credential encoding");
    assert_eq!(
        encoding_error.kind(),
        CredentialLeakErrorKind::RecognizedEncoding
    );

    let tracked = secret_fingerprint_gate(&secret(USER_SECRET));

    // The durable credential reference aggregate keeps identity metadata
    // only.
    let state = stored_credential_state(&mut storage, &id);
    assert_secret_free(&tracked, &state.payload);

    // The lifecycle event and the pending audit payload of the create are
    // secret-free, too.
    let receipt = credential_receipt(&mut storage, &id, 1);
    for event in &receipt.events {
        assert_secret_free(&tracked, &event.payload);
    }
    if let Some(audit) = storage
        .load_pending_audit_event(&receipt.receipt_identity)
        .expect("pending audit reads")
    {
        assert_secret_free(&tracked, audit.payload());
    }

    // The returned response values never carry the secret.
    let response_bytes = serde_json::to_vec(&onboarded).expect("response serializes");
    assert_secret_free(&tracked, &response_bytes);

    // Nothing on the durable surface — database, write-ahead log, or the
    // returned values — holds the plaintext. The dedicated secret store
    // directory is excluded from the disk scan because storing the secret is
    // exactly its job; it is asserted separately below.
    for bytes in collect_file_bytes(&harness.data_directory, Some(&harness.secret_directory)) {
        assert_secret_free(&tracked, &bytes);
    }
    let version_files = secret_files(&harness.secret_directory)
        .into_iter()
        .filter(|(name, _)| name.starts_with("version-"))
        .collect::<Vec<_>>();
    let [(_, stored_bytes)] = version_files.as_slice() else {
        panic!("exactly one secret version file expected");
    };
    assert_eq!(stored_bytes.as_slice(), USER_SECRET.as_bytes());

    // And error messages stay secret-free as well: an unknown reference
    // rejects the rotation without ever naming a secret.
    let mut rotating = ProviderOnboardingService::new(&mut storage, &store, &harness.probe);
    let error = rotating
        .rotate_credential(RotateCredentialRequest {
            actor: actor(),
            organization_scope: organization_scope(),
            credential_reference_id: unknown_reference_id(),
            next_secret: secret(ROTATED_SECRET),
        })
        .expect_err("an unknown reference rejects the rotation");
    assert!(!error.message().contains(USER_SECRET));
    assert!(!error.message().contains(ROTATED_SECRET));

    drop(storage);
    let _ = std::fs::remove_dir_all(&harness.data_directory);
}

fn collect_file_bytes(directory: &Path, exclude: Option<&Path>) -> Vec<Vec<u8>> {
    let mut files = Vec::new();
    let mut stack = vec![directory.to_path_buf()];
    while let Some(current) = stack.pop() {
        if exclude.is_some_and(|exclude| current.starts_with(exclude)) {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&current) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if let Ok(bytes) = std::fs::read(&path) {
                files.push(bytes);
            }
        }
    }
    files
}

/// A canonical identity for a reference that was never created; used only
/// to drive negative paths, never as a flow input.
fn unknown_reference_id() -> CredentialReferenceId {
    CredentialReferenceId(format!("crd_{}", "Z".repeat(26)))
}

/// Lists the credential references durable in the scope; deleted references
/// are absent from the catalog.
fn list_reference_ids(storage: &mut SqliteStorage) -> Vec<String> {
    let response = CredentialReferenceService::new(storage)
        .list(&CredentialReferenceListQuery {
            actor: actor(),
            page: PageRequest {
                cursor: None,
                limit: 50,
            },
            parameters: CredentialReferenceListParameters { provider_id: None },
            query: CredentialReferenceListQueryQuery::CredentialReferenceList,
            request_id: RequestId(format!("req_{}", "A".repeat(26))),
            schema_version: SchemaVersion::WinwincodeV1,
            scope: scope(),
        })
        .expect("reference list reads");
    response
        .result
        .items
        .iter()
        .map(|item| item.id.0.clone())
        .collect()
}

#[test]
fn failed_connection_test_creates_no_route_and_cleans_the_reference() {
    let harness = harness("test-fail-no-route", FakeProbe::unreachable());
    let mut storage = SqliteStorage::open(&harness.data_directory).expect("storage opens");
    let store = secret_store(&harness);
    let mut service = ProviderOnboardingService::new(&mut storage, &store, &harness.probe);
    let error = service
        .onboard_provider(OnboardProviderRequest {
            actor: actor(),
            organization_scope: organization_scope(),
            provider_id: PROVIDER.to_owned(),
            custom_endpoint: None,
            secret: secret(USER_SECRET),
            default_model_id: Some("deepseek-chat".to_owned()),
        })
        .expect_err("a failed connection test fails the onboarding");
    assert_eq!(error.kind(), ProviderOnboardingErrorKind::ConnectionFailed);

    // No half-created reference remains in the scope catalog.
    let references = list_reference_ids(&mut storage);
    assert!(
        references.is_empty(),
        "no reference may remain, found: {references:?}"
    );

    // No route and no provider registration exist.
    let catalog = ProviderCatalogService::new(&mut storage)
        .project(&scope())
        .expect("catalog projects");
    assert_eq!(catalog.catalog_version, 0);
    assert!(catalog.providers.is_empty());
    let settings = ModelSettingsService::new(&mut storage)
        .project(&ModelSettingsTarget::Organization {
            scope: organization_scope(),
        })
        .expect("settings project");
    assert!(settings.default_model_route.is_none());

    // No stored secret material remains.
    assert_no_version_files(&harness.secret_directory);

    drop(storage);
    let _ = std::fs::remove_dir_all(&harness.data_directory);
}

#[test]
fn route_establishment_requires_probe_confirmed_capabilities() {
    // The probe authenticates but confirms no capabilities: the preset model
    // list must show unknown capabilities and no route may be established.
    let harness = harness("no-capabilities", FakeProbe::confirmed(Vec::new()));
    let mut storage = SqliteStorage::open(&harness.data_directory).expect("storage opens");
    let store = secret_store(&harness);
    let mut service = ProviderOnboardingService::new(&mut storage, &store, &harness.probe);
    let created = service
        .create_credential_reference(CreateCredentialRequest {
            actor: actor(),
            organization_scope: organization_scope(),
            provider_id: PROVIDER.to_owned(),
            custom_endpoint: None,
            secret: secret(USER_SECRET),
        })
        .expect("credential creates");
    let report = service
        .test_connection(&TestConnectionRequest {
            actor: actor(),
            organization_scope: organization_scope(),
            provider_id: PROVIDER.to_owned(),
            custom_endpoint: None,
            credential_reference_id: created.credential_reference_id.clone(),
        })
        .expect("a reachable endpoint reports a result");
    assert!(report.connection_succeeded());
    let model_ids: Vec<&str> = report
        .models()
        .iter()
        .map(|entry| entry.model_id.as_str())
        .collect();
    assert_eq!(model_ids, ["deepseek-chat", "deepseek-reasoner"]);
    for entry in report.models() {
        assert!(
            entry.capability.is_none(),
            "unconfirmed capabilities must stay unknown, never fabricated"
        );
    }

    let error = service
        .establish_model_route(
            &report,
            &EstablishRouteRequest {
                actor: actor(),
                organization_scope: organization_scope(),
                default_model_id: Some("deepseek-chat".to_owned()),
            },
        )
        .expect_err("no route without verified capabilities");
    assert_eq!(
        error.kind(),
        ProviderOnboardingErrorKind::ConnectionTestRequired
    );
    let catalog = ProviderCatalogService::new(&mut storage)
        .project(&scope())
        .expect("catalog projects");
    assert_eq!(catalog.catalog_version, 0);

    drop(storage);
    let _ = std::fs::remove_dir_all(&harness.data_directory);
}

#[test]
fn probe_reports_violating_catalog_rules_are_rejected() {
    let invalid = vec![ModelCapability {
        model_id: "deepseek-chat".to_owned(),
        display_name: "DeepSeek Chat".to_owned(),
        context_window_tokens: 8_000,
        max_output_tokens: 16_000,
        tool_support: ModelToolSupport::Parallel,
        reasoning_efforts: vec![],
    }];
    let harness = harness("invalid-report", FakeProbe::confirmed(invalid));
    let mut storage = SqliteStorage::open(&harness.data_directory).expect("storage opens");
    let store = secret_store(&harness);
    let mut service = ProviderOnboardingService::new(&mut storage, &store, &harness.probe);
    let created = service
        .create_credential_reference(CreateCredentialRequest {
            actor: actor(),
            organization_scope: organization_scope(),
            provider_id: PROVIDER.to_owned(),
            custom_endpoint: None,
            secret: secret(USER_SECRET),
        })
        .expect("credential creates");
    let error = service
        .test_connection(&TestConnectionRequest {
            actor: actor(),
            organization_scope: organization_scope(),
            provider_id: PROVIDER.to_owned(),
            custom_endpoint: None,
            credential_reference_id: created.credential_reference_id.clone(),
        })
        .expect_err("an impossible capability report is rejected");
    assert_eq!(
        error.kind(),
        ProviderOnboardingErrorKind::ProbeReportInvalid
    );
    let catalog = ProviderCatalogService::new(&mut storage)
        .project(&scope())
        .expect("catalog projects");
    assert_eq!(catalog.catalog_version, 0, "no descriptor was built");

    drop(storage);
    let _ = std::fs::remove_dir_all(&harness.data_directory);
}

#[test]
fn rotation_switches_material_and_scrubs_the_old_version() {
    let harness = harness(
        "rotation",
        FakeProbe::confirmed(confirmed_deepseek_models()),
    );
    let mut storage = SqliteStorage::open(&harness.data_directory).expect("storage opens");
    let store = secret_store(&harness);
    let (reference, _) = onboard(&harness, &mut storage);
    let id = reference.credential_reference_id.clone();

    let old = CredentialReferenceService::new(&mut storage)
        .resolve_secret(&store, &scope(), &id)
        .expect("old secret resolves");
    assert_eq!(old.expose(), USER_SECRET.as_bytes());

    let rotated = ProviderOnboardingService::new(&mut storage, &store, &harness.probe)
        .rotate_credential(RotateCredentialRequest {
            actor: actor(),
            organization_scope: organization_scope(),
            credential_reference_id: id.clone(),
            next_secret: secret(ROTATED_SECRET),
        })
        .expect("rotation commits");
    assert_eq!(rotated.previous_rotation_version, 1);
    assert_eq!(rotated.rotation_version, 2);
    assert!(rotated.cleanup_completed, "old material is scrubbed");

    // The new material is authoritative everywhere.
    let new = CredentialReferenceService::new(&mut storage)
        .resolve_secret(&store, &scope(), &id)
        .expect("new secret resolves");
    assert_eq!(new.expose(), ROTATED_SECRET.as_bytes());

    // Exactly one immutable version file remains, holding the new secret.
    let files = secret_files(&harness.secret_directory);
    let versions: Vec<&(String, Vec<u8>)> = files
        .iter()
        .filter(|(name, _)| name.starts_with("version-"))
        .collect();
    let [(_, bytes)] = versions.as_slice() else {
        panic!("exactly one version file expected, got {versions:?}");
    };
    assert_eq!(bytes.as_slice(), ROTATED_SECRET.as_bytes());

    // The rotation is auditable: revision two carries the rotate lifecycle
    // event and its pending audit record.
    let receipt = credential_receipt(&mut storage, &id, 2);
    let rotated_event = receipt
        .events
        .iter()
        .find(|event| {
            event
                .payload
                .windows(ROTATED_MARKER.len())
                .any(|window| window == ROTATED_MARKER)
        })
        .expect("rotate lifecycle event exists");
    assert!(!rotated_event.event_id.is_empty());
    let audit = storage
        .load_pending_audit_event(&receipt.receipt_identity)
        .expect("audit reads")
        .expect("rotation audit exists");
    assert!(
        audit
            .payload()
            .windows(ROTATE_ACTION.len())
            .any(|window| window == ROTATE_ACTION),
        "the audit record names the rotation action"
    );

    // The route keeps pointing at the same reference and keeps working.
    let catalog = ProviderCatalogService::new(&mut storage)
        .project(&scope())
        .expect("catalog projects");
    let [provider] = catalog.providers.as_slice() else {
        panic!("exactly one provider expected");
    };
    assert_eq!(&provider.credential_reference_id, &id);
    assert_eq!(provider.availability, CatalogAvailability::Enabled);

    drop(storage);
    let _ = std::fs::remove_dir_all(&harness.data_directory);
}

#[test]
fn failed_rotation_leaves_the_previous_credential_authoritative() {
    let harness = harness(
        "rotation-stage-fail",
        FakeProbe::confirmed(confirmed_deepseek_models()),
    );
    let mut storage = SqliteStorage::open(&harness.data_directory).expect("storage opens");
    let store = secret_store(&harness);
    let (reference, _) = onboard(&harness, &mut storage);
    let id = reference.credential_reference_id.clone();

    let failing = FailStageStore::new(secret_store(&harness));
    let error = ProviderOnboardingService::new(&mut storage, &failing, &harness.probe)
        .rotate_credential(RotateCredentialRequest {
            actor: actor(),
            organization_scope: organization_scope(),
            credential_reference_id: id.clone(),
            next_secret: secret(ROTATED_SECRET),
        })
        .expect_err("a staging failure aborts the rotation");
    assert_eq!(
        error.kind(),
        ProviderOnboardingErrorKind::SecretStore(SecretStoreErrorKind::Unavailable)
    );

    // The previous credential still resolves and the metadata never moved.
    let current = CredentialReferenceService::new(&mut storage)
        .resolve_secret(&store, &scope(), &id)
        .expect("old secret still resolves");
    assert_eq!(current.expose(), USER_SECRET.as_bytes());
    let resolution = CredentialReferenceService::new(&mut storage)
        .resolve(&scope(), &id)
        .expect("reference resolves");
    assert_eq!(resolution.rotation_version(), 1);

    // The route is untouched.
    let catalog = ProviderCatalogService::new(&mut storage)
        .project(&scope())
        .expect("catalog projects");
    let [provider] = catalog.providers.as_slice() else {
        panic!("exactly one provider expected");
    };
    assert_eq!(&provider.credential_reference_id, &id);

    drop(storage);
    let _ = std::fs::remove_dir_all(&harness.data_directory);
}

#[test]
fn occupied_next_version_rejection_keeps_the_old_credential_authoritative() {
    let harness = harness(
        "rotation-conflict",
        FakeProbe::confirmed(confirmed_deepseek_models()),
    );
    let mut storage = SqliteStorage::open(&harness.data_directory).expect("storage opens");
    let store = secret_store(&harness);
    let (reference, _) = onboard(&harness, &mut storage);
    let id = reference.credential_reference_id.clone();

    // A concurrent rotation already staged different material at the next
    // version without committing metadata.
    let current = CredentialReferenceService::new(&mut storage)
        .resolve(&scope(), &id)
        .expect("reference resolves");
    store
        .rotate(&current, secret("sk-sneaky-concurrent-0123456789abcdef"))
        .expect("the concurrent stage occupies version two");

    let error = ProviderOnboardingService::new(&mut storage, &store, &harness.probe)
        .rotate_credential(RotateCredentialRequest {
            actor: actor(),
            organization_scope: organization_scope(),
            credential_reference_id: id.clone(),
            next_secret: secret(ROTATED_SECRET),
        })
        .expect_err("a different value already occupies the next version");
    assert_eq!(
        error.kind(),
        ProviderOnboardingErrorKind::SecretStore(SecretStoreErrorKind::VersionConflict)
    );

    // The previous credential stays authoritative and the metadata never
    // moved; the foreign staged version is not this flow's material to
    // scrub, and it is inert because resolution still selects version one.
    let resolution = CredentialReferenceService::new(&mut storage)
        .resolve(&scope(), &id)
        .expect("reference resolves");
    assert_eq!(resolution.rotation_version(), 1);
    let resolved = CredentialReferenceService::new(&mut storage)
        .resolve_secret(&store, &scope(), &id)
        .expect("old secret still resolves");
    assert_eq!(resolved.expose(), USER_SECRET.as_bytes());
    let authoritative = secret_files(&harness.secret_directory)
        .into_iter()
        .filter(|(name, _)| name == "version-00000000000000000001.secret")
        .collect::<Vec<_>>();
    let [(_, bytes)] = authoritative.as_slice() else {
        panic!("the authoritative version file must remain");
    };
    assert_eq!(bytes.as_slice(), USER_SECRET.as_bytes());

    drop(storage);
    let _ = std::fs::remove_dir_all(&harness.data_directory);
}

#[test]
fn create_failure_after_secret_store_rejection_leaves_no_reference() {
    let harness = harness(
        "create-fail",
        FakeProbe::confirmed(confirmed_deepseek_models()),
    );
    let mut storage = SqliteStorage::open(&harness.data_directory).expect("storage opens");
    let rejecting = RejectingStore;
    let error = ProviderOnboardingService::new(&mut storage, &rejecting, &harness.probe)
        .create_credential_reference(CreateCredentialRequest {
            actor: actor(),
            organization_scope: organization_scope(),
            provider_id: PROVIDER.to_owned(),
            custom_endpoint: None,
            secret: secret(USER_SECRET),
        })
        .expect_err("a secret store failure fails the creation");
    assert_eq!(
        error.kind(),
        ProviderOnboardingErrorKind::SecretStore(SecretStoreErrorKind::Unavailable)
    );

    // The half-created reference was tombstoned: nothing is listed in the
    // scope catalog, and no route or stored material exists.
    let references = list_reference_ids(&mut storage);
    assert!(
        references.is_empty(),
        "no reference may remain, found: {references:?}"
    );
    let catalog = ProviderCatalogService::new(&mut storage)
        .project(&scope())
        .expect("catalog projects");
    assert_eq!(catalog.catalog_version, 0);
    assert!(catalog.providers.is_empty());
    assert_no_version_files(&harness.secret_directory);

    drop(storage);
    let _ = std::fs::remove_dir_all(&harness.data_directory);
}

#[test]
fn custom_endpoints_pass_canonical_https_validation_before_any_write() {
    let model = vec![ModelCapability {
        model_id: "relay-large".to_owned(),
        display_name: "Relay Large".to_owned(),
        context_window_tokens: 64_000,
        max_output_tokens: 4_000,
        tool_support: ModelToolSupport::Serial,
        reasoning_efforts: vec![],
    }];
    let harness = harness("custom-endpoint", FakeProbe::confirmed(model));
    let mut storage = SqliteStorage::open(&harness.data_directory).expect("storage opens");
    let store = secret_store(&harness);
    let mut service = ProviderOnboardingService::new(&mut storage, &store, &harness.probe);
    let onboarded = service
        .onboard_provider(OnboardProviderRequest {
            actor: actor(),
            organization_scope: organization_scope(),
            provider_id: "relay-custom".to_owned(),
            custom_endpoint: Some("https://relay.example.internal/v1".to_owned()),
            secret: secret(USER_SECRET),
            default_model_id: None,
        })
        .expect("custom endpoint onboarding reaches ready");
    // With no default model supplied, the single confirmed model is selected.
    assert_eq!(onboarded.route.model_id, "relay-large");
    assert_eq!(
        onboarded.route.provider_id, "relay-custom",
        "the route is established from the verified report"
    );
    let catalog = ProviderCatalogService::new(&mut storage)
        .project(&scope())
        .expect("catalog projects");
    let [provider] = catalog.providers.as_slice() else {
        panic!("exactly one provider expected");
    };
    assert_eq!(provider.adapter_kind, "openai-responses");
    assert_eq!(provider.display_name, "relay-custom");

    // Non-HTTPS endpoints are rejected before any state is written.
    let rejected_store = secret_store(&harness);
    let mut rejected =
        ProviderOnboardingService::new(&mut storage, &rejected_store, &harness.probe);
    let error = rejected
        .create_credential_reference(CreateCredentialRequest {
            actor: actor(),
            organization_scope: organization_scope(),
            provider_id: "relay-custom".to_owned(),
            custom_endpoint: Some("http://relay.example.internal/v1".to_owned()),
            secret: secret(USER_SECRET),
        })
        .expect_err("plaintext endpoints are invalid");
    assert_eq!(error.kind(), ProviderOnboardingErrorKind::InvalidRequest);

    // A custom identity without a preset and without an endpoint is unknown.
    let unknown_store = secret_store(&harness);
    let mut unknown = ProviderOnboardingService::new(&mut storage, &unknown_store, &harness.probe);
    let error = unknown
        .test_connection(&TestConnectionRequest {
            actor: actor(),
            organization_scope: organization_scope(),
            provider_id: "relay-unknown".to_owned(),
            custom_endpoint: None,
            credential_reference_id: onboarded
                .credential_reference
                .credential_reference_id
                .clone(),
        })
        .expect_err("an unknown provider without an endpoint is rejected");
    assert_eq!(error.kind(), ProviderOnboardingErrorKind::ProviderNotFound);

    drop(storage);
    let _ = std::fs::remove_dir_all(&harness.data_directory);
}

#[test]
fn a_failed_test_can_be_retried_without_recreating_the_credential() {
    let harness = harness("retry", FakeProbe::unreachable());
    let mut storage = SqliteStorage::open(&harness.data_directory).expect("storage opens");
    let store = secret_store(&harness);
    let mut service = ProviderOnboardingService::new(&mut storage, &store, &harness.probe);
    let created = service
        .create_credential_reference(CreateCredentialRequest {
            actor: actor(),
            organization_scope: organization_scope(),
            provider_id: PROVIDER.to_owned(),
            custom_endpoint: None,
            secret: secret(USER_SECRET),
        })
        .expect("credential creates");
    let failed = service
        .test_connection(&TestConnectionRequest {
            actor: actor(),
            organization_scope: organization_scope(),
            provider_id: PROVIDER.to_owned(),
            custom_endpoint: None,
            credential_reference_id: created.credential_reference_id.clone(),
        })
        .expect("a failed probe reports a result");
    assert!(!failed.connection_succeeded());

    harness
        .probe
        .set_outcome(ProbeOutcome::connected(confirmed_deepseek_models()));
    let verified = service
        .test_connection(&TestConnectionRequest {
            actor: actor(),
            organization_scope: organization_scope(),
            provider_id: PROVIDER.to_owned(),
            custom_endpoint: None,
            credential_reference_id: created.credential_reference_id.clone(),
        })
        .expect("the retried probe verifies the endpoint");
    assert!(verified.connection_succeeded());
    let established = service
        .establish_model_route(
            &verified,
            &EstablishRouteRequest {
                actor: actor(),
                organization_scope: organization_scope(),
                default_model_id: Some("deepseek-reasoner".to_owned()),
            },
        )
        .expect("the verified report establishes the route");
    assert_eq!(established.model_id, "deepseek-reasoner");
    assert_eq!(
        &established.credential_reference_id,
        &created.credential_reference_id
    );

    drop(storage);
    let _ = std::fs::remove_dir_all(&harness.data_directory);
}

#[test]
fn the_capability_source_reports_only_confirmed_values() {
    let harness = harness(
        "capability-source",
        FakeProbe::confirmed(confirmed_deepseek_models()),
    );
    let mut storage = SqliteStorage::open(&harness.data_directory).expect("storage opens");
    let store = secret_store(&harness);
    let mut service = ProviderOnboardingService::new(&mut storage, &store, &harness.probe);
    let created = service
        .create_credential_reference(CreateCredentialRequest {
            actor: actor(),
            organization_scope: organization_scope(),
            provider_id: PROVIDER.to_owned(),
            custom_endpoint: None,
            secret: secret(USER_SECRET),
        })
        .expect("credential creates");
    let report: ConnectionTestReport = service
        .test_connection(&TestConnectionRequest {
            actor: actor(),
            organization_scope: organization_scope(),
            provider_id: PROVIDER.to_owned(),
            custom_endpoint: None,
            credential_reference_id: created.credential_reference_id.clone(),
        })
        .expect("the probe verifies the endpoint");

    let confirmed = report
        .capability(PROVIDER, "deepseek-chat")
        .expect("probe-confirmed values are reported");
    assert_eq!(confirmed.context_window_tokens, 128_000);
    assert!(report.capability(PROVIDER, "unlisted-model").is_none());
    assert!(
        report
            .capability("other-provider", "deepseek-chat")
            .is_none()
    );

    drop(storage);
    let _ = std::fs::remove_dir_all(&harness.data_directory);
}
