// SPDX-License-Identifier: Apache-2.0

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::json;
use winwincode_api::generated::{
    Actor, CredentialReferenceCreateCommand, CredentialReferenceCreateCommandCommand,
    CredentialReferenceCreatePayload, OrganizationScope, OrganizationScopeKind, Scope, UserActor,
    UserActorKind,
};
use winwincode_control_plane::{
    CredentialLeakErrorKind, CredentialLeakGate, CredentialOutputBoundary,
    CredentialReferenceService, LocalSecretStoreAdapter, ProductStateStorage, ResolvedSecret,
};
use winwincode_domain::{
    CredentialReferenceId, OrganizationId, RequestId, Revision, SchemaVersion, UserId,
};
use winwincode_storage::SqliteStorage;

const EXACT_SECRET: &[u8] = b"fixture exact secret with no recognized token syntax";
const CONFLICT_SECRET: &[u8] = b"different fixture value";

static NEXT_TEMP_DIRECTORY: AtomicU64 = AtomicU64::new(1);

fn temporary_directory() -> PathBuf {
    let suffix = NEXT_TEMP_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "winwincode-credential-leak-gate-{}-{suffix}",
        std::process::id()
    ))
}

fn id(prefix: &str, seed: u64) -> String {
    format!("{prefix}_{seed:026}")
}

fn create_command() -> CredentialReferenceCreateCommand {
    CredentialReferenceCreateCommand {
        actor: Actor::UserActor(UserActor {
            id: UserId(id("usr", 1)),
            kind: UserActorKind::User,
        }),
        command: CredentialReferenceCreateCommandCommand::CredentialReferenceCreate,
        expected_revision: Revision(0),
        payload: CredentialReferenceCreatePayload {
            credential_reference_id: CredentialReferenceId(id("crd", 1)),
            display_name: "Provider credential".to_owned(),
            provider_id: "provider-main".to_owned(),
            vault_locator: format!("local-fixture://{}", String::from_utf8_lossy(EXACT_SECRET)),
        },
        request_id: RequestId(id("req", 1)),
        schema_version: SchemaVersion::WinwincodeV1,
        scope: Scope::OrganizationScope(OrganizationScope {
            kind: OrganizationScopeKind::Organization,
            organization_id: OrganizationId(id("org", 1)),
        }),
    }
}

fn boundaries() -> [CredentialOutputBoundary; 12] {
    [
        CredentialOutputBoundary::Log,
        CredentialOutputBoundary::Error,
        CredentialOutputBoundary::Debug,
        CredentialOutputBoundary::Serialization,
        CredentialOutputBoundary::Persistence,
        CredentialOutputBoundary::Event,
        CredentialOutputBoundary::Audit,
        CredentialOutputBoundary::Artifact,
        CredentialOutputBoundary::Evidence,
        CredentialOutputBoundary::Http,
        CredentialOutputBoundary::WebSocket,
        CredentialOutputBoundary::ReleasePackage,
    ]
}

#[test]
fn exact_fingerprints_and_field_policy_fail_closed_with_secret_free_diagnostics() {
    let secret = ResolvedSecret::from_bytes(EXACT_SECRET.to_vec()).expect("fixture secret");
    let mut gate = CredentialLeakGate::new();
    gate.track_secret(&secret);

    for boundary in boundaries() {
        let leaked = [b"safe-prefix:".as_slice(), EXACT_SECRET].concat();
        let error = gate
            .inspect_bytes(boundary, &leaked)
            .expect_err("exact secret must be rejected at every output boundary");
        assert_eq!(error.boundary(), boundary);
        assert_eq!(error.kind(), CredentialLeakErrorKind::ExactSecret);
        let diagnostic = format!("{error:?} {error}");
        assert!(!diagnostic.contains(String::from_utf8_lossy(EXACT_SECRET).as_ref()));
    }

    let forbidden = gate
        .inspect_serializable(
            CredentialOutputBoundary::Http,
            &json!({ "vaultLocator": "local-fixture://value" }),
        )
        .expect_err("write-only locator cannot cross an output boundary");
    assert_eq!(forbidden.kind(), CredentialLeakErrorKind::ForbiddenField);
    let recognized = CredentialLeakGate::new()
        .inspect_bytes(
            CredentialOutputBoundary::Log,
            b"provider failed with Bearer abcdefghijklmnop",
        )
        .expect_err("recognized bearer syntax must be rejected");
    assert_eq!(
        recognized.kind(),
        CredentialLeakErrorKind::RecognizedEncoding
    );
    assert_eq!(
        gate.inspect_json_bytes(CredentialOutputBoundary::Evidence, b"{not-json")
            .expect_err("malformed JSON fails closed")
            .kind(),
        CredentialLeakErrorKind::InvalidOutput
    );
}

#[test]
fn provider_token_detection_respects_value_boundaries() {
    let gate = CredentialLeakGate::new();
    gate.inspect_bytes(
        CredentialOutputBoundary::WebSocket,
        b"delivery-task-breakdown-transaction",
    )
    .expect("public component names must not be rejected for an embedded sk- substring");

    for token in [
        "sk-abcdefghijklmnop",
        "provider returned sk-abcdefghijklmnop",
        "provider=sk-abcdefghijklmnop",
    ] {
        assert_eq!(
            gate.inspect_bytes(CredentialOutputBoundary::WebSocket, token.as_bytes())
                .expect_err("provider token at a value boundary must be rejected")
                .kind(),
            CredentialLeakErrorKind::RecognizedEncoding
        );
    }
}

#[test]
fn credential_reference_flow_passes_only_references_and_stable_diagnostics_to_outputs() {
    let root = temporary_directory();
    let mut storage = SqliteStorage::open(root.join("metadata")).expect("open metadata storage");
    let command = create_command();
    let response = CredentialReferenceService::new(&mut storage)
        .create(&command, 1_800_000_000_000)
        .expect("create Credential reference metadata");
    let reference = CredentialReferenceService::new(&mut storage)
        .resolve(&command.scope, &command.payload.credential_reference_id)
        .expect("resolve Credential reference");

    let secret = ResolvedSecret::from_bytes(EXACT_SECRET.to_vec()).expect("fixture secret");
    let mut gate = CredentialLeakGate::new();
    gate.track_secret(&secret);
    let adapter = LocalSecretStoreAdapter::open(root.join("secrets")).expect("open SecretStore");
    let receipt = adapter
        .store(&reference, secret)
        .expect("store fixture secret behind its reference");

    gate.inspect_serializable(CredentialOutputBoundary::Http, &response)
        .expect("typed HTTP response is secret-free");
    gate.inspect_serializable(CredentialOutputBoundary::WebSocket, &response)
        .expect("typed WebSocket projection is secret-free");
    gate.inspect_bytes(
        CredentialOutputBoundary::Debug,
        format!("{receipt:?}").as_bytes(),
    )
    .expect("write receipt Debug is secret-free");
    gate.inspect_bytes(
        CredentialOutputBoundary::Log,
        b"Credential reference create completed",
    )
    .expect("stable log line is secret-free");

    let stored = storage
        .load_state(&format!(
            "credential-reference:{}",
            command.payload.credential_reference_id.0
        ))
        .expect("load state")
        .expect("state exists");
    gate.inspect_json_bytes(CredentialOutputBoundary::Persistence, &stored.payload)
        .expect("persistent state is secret-free");
    for event in storage.pending_events().expect("load events") {
        gate.inspect_json_bytes(CredentialOutputBoundary::Event, &event.payload)
            .expect("event is secret-free");
    }
    for audit in storage.pending_audit_events().expect("load audit") {
        gate.inspect_json_bytes(CredentialOutputBoundary::Audit, audit.payload())
            .expect("audit is secret-free");
    }

    for (boundary, output) in [
        (
            CredentialOutputBoundary::Artifact,
            json!({ "artifactId": "art_00000000000000000000000001", "sizeBytes": 42 }),
        ),
        (
            CredentialOutputBoundary::Evidence,
            json!({ "manifest": "evidence-v1", "credentialReferenceId": reference.credential_reference_id() }),
        ),
        (
            CredentialOutputBoundary::Serialization,
            serde_json::to_value(&reference).expect("serialize resolution"),
        ),
    ] {
        gate.inspect_serializable(boundary, &output)
            .expect("typed boundary output is secret-free");
    }
    gate.inspect_bytes(
        CredentialOutputBoundary::ReleasePackage,
        b"release manifest contains references only",
    )
    .expect("release manifest is secret-free");

    let conflict = adapter
        .store(
            &reference,
            ResolvedSecret::from_bytes(CONFLICT_SECRET.to_vec()).expect("conflict fixture"),
        )
        .expect_err("immutable secret version rejects different bytes");
    gate.inspect_bytes(
        CredentialOutputBoundary::Error,
        format!("{conflict:?} {conflict}").as_bytes(),
    )
    .expect("stable error remains diagnostic and secret-free");

    adapter.delete(&reference).expect("delete secret fixture");
    Box::new(storage).close().expect("close metadata storage");
    fs::remove_dir_all(root).expect("remove leak-gate fixture");
}

fn strongflow_cursor(token: &str) -> serde_json::Value {
    let scope = json!({
        "kind": "repository",
        "organizationId": id("org", 1),
        "projectId": id("prj", 1),
        "repositoryId": id("repo", 1),
        "workspaceId": id("wsp", 1),
    });
    json!({
        "deliveryId": id("dlv", 1),
        "deliveryRevision": 1,
        "eventCursor": {
            "eventId": null,
            "scope": scope.clone(),
            "sequence": 0,
            "stream": { "deliveryId": id("dlv", 1), "kind": "delivery" },
        },
        "publicationRevision": 1,
        "runtimeAcceptedSequence": 0,
        "runtimeLedgerRevision": 1,
        "scope": scope,
        "token": token,
    })
}

#[test]
fn only_the_exact_canonical_strongflow_cursor_token_field_is_public() {
    let token = format!("sfc1_{}", "a".repeat(64));
    let gate = CredentialLeakGate::new();
    gate.inspect_serializable(
        CredentialOutputBoundary::Http,
        &json!({ "readCursor": strongflow_cursor(&token) }),
    )
    .expect("canonical generated StrongFlow cursor is public");

    for invalid in [
        json!({ "readCursor": { "token": token.clone() } }),
        {
            let mut cursor = strongflow_cursor(&token);
            cursor["extra"] = json!(true);
            json!({ "readCursor": cursor })
        },
        {
            let mut cursor = strongflow_cursor(&token);
            cursor["scope"]["kind"] = json!("organization");
            json!({ "readCursor": cursor })
        },
        json!({ "readCursor": strongflow_cursor("sfc1_short") }),
        json!({ "ordinary": { "token": token.clone() } }),
    ] {
        assert_eq!(
            gate.inspect_serializable(CredentialOutputBoundary::Http, &invalid)
                .expect_err("non-canonical token field stays forbidden")
                .kind(),
            CredentialLeakErrorKind::ForbiddenField
        );
    }

    let secret = ResolvedSecret::from_bytes(token.as_bytes().to_vec()).expect("cursor secret");
    let mut tracked = CredentialLeakGate::new();
    tracked.track_secret(&secret);
    assert_eq!(
        tracked
            .inspect_serializable(
                CredentialOutputBoundary::Http,
                &json!({ "readCursor": strongflow_cursor(&token) }),
            )
            .expect_err("tracked secret remains forbidden even in a canonical cursor")
            .kind(),
        CredentialLeakErrorKind::ExactSecret
    );
}
