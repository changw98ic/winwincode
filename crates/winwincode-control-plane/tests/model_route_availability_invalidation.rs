// SPDX-License-Identifier: Apache-2.0

use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

use serde_json::Value;
use winwincode_api::generated::{
    Actor, ControlPlaneWebSocketModelRouteAvailabilityInvalidatedEvent,
    ControlPlaneWebSocketModelRouteAvailabilityInvalidationSource,
    CredentialReferenceCreateCommand, CredentialReferenceCreateCommandCommand,
    CredentialReferenceCreatePayload, OrganizationScope, OrganizationScopeKind, Scope, UserActor,
    UserActorKind,
};
use winwincode_control_plane::{
    CredentialReferenceService, ModelCapability, ModelSettingsRequest, ModelSettingsService,
    ModelSettingsTarget, ModelSettingsValues, ModelToolSupport, ProviderCatalogRequest,
    ProviderCatalogService, ProviderDescriptor,
};
use winwincode_domain::{
    CredentialReferenceId, Instant, OrganizationId, RequestId, Revision, SchemaVersion, UserId,
};
use winwincode_storage::{ProductStateStorage, PublicEventScope, SqliteStorage};

static NEXT_TEMP_DIRECTORY: AtomicU64 = AtomicU64::new(1);
const INVALIDATION_TOPIC: &str = "model-route-availability.invalidated.v1";

fn temporary_directory() -> PathBuf {
    let suffix = NEXT_TEMP_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "winwincode-model-route-invalidation-{}-{suffix}",
        std::process::id()
    ))
}

fn id(prefix: &str, seed: u64) -> String {
    format!("{prefix}_{seed:026}")
}

fn actor() -> Actor {
    Actor::UserActor(UserActor {
        id: UserId(id("usr", 1)),
        kind: UserActorKind::User,
    })
}

fn scope(seed: u64) -> Scope {
    Scope::OrganizationScope(OrganizationScope {
        kind: OrganizationScopeKind::Organization,
        organization_id: OrganizationId(id("org", seed)),
    })
}

fn at() -> Instant {
    Instant("2032-09-02T08:00:00.000Z".to_owned())
}

fn public_invalidations(
    storage: &SqliteStorage,
) -> Vec<(
    ControlPlaneWebSocketModelRouteAvailabilityInvalidatedEvent,
    PublicEventScope,
)> {
    storage
        .pending_events()
        .expect("pending outbox")
        .into_iter()
        .filter(|event| event.topic == INVALIDATION_TOPIC)
        .map(|event| {
            let value: Value = serde_json::from_slice(&event.payload).expect("event JSON");
            let keys = value
                .as_object()
                .expect("event object")
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>();
            assert_eq!(keys, ["reloadQueries", "source", "sourceRevision", "type"]);
            let decoded = serde_json::from_value(value).expect("generated invalidation");
            let context = event.public_context.expect("durable public context");
            (decoded, context.scope().clone())
        })
        .collect()
}

#[test]
#[allow(clippy::too_many_lines)]
fn settings_catalog_and_credential_changes_publish_closed_durable_invalidations_once() {
    let root = temporary_directory();
    let authority_scope = scope(1);
    let provider_request = ProviderCatalogRequest {
        actor: actor(),
        scope: authority_scope.clone(),
        request_id: RequestId(id("req", 1)),
        expected_catalog_version: 0,
    };
    let descriptor = ProviderDescriptor {
        provider_id: "provider-safe".to_owned(),
        display_name: "Provider Safe".to_owned(),
        adapter_kind: "fixture-adapter".to_owned(),
        credential_reference_id: CredentialReferenceId(id("crd", 1)),
        models: vec![ModelCapability {
            model_id: "model-safe".to_owned(),
            display_name: "Model Safe".to_owned(),
            context_window_tokens: 8_192,
            max_output_tokens: 2_048,
            tool_support: ModelToolSupport::Serial,
            reasoning_efforts: vec!["medium".to_owned()],
        }],
    };
    let settings_request = ModelSettingsRequest {
        actor: actor(),
        target: ModelSettingsTarget::Organization {
            scope: OrganizationScope {
                kind: OrganizationScopeKind::Organization,
                organization_id: OrganizationId(id("org", 1)),
            },
        },
        request_id: RequestId(id("req", 2)),
        expected_revision: 0,
    };
    let settings = ModelSettingsValues {
        default_model_route: None,
        worker_concurrency_limit: 2,
    };
    let credential = CredentialReferenceCreateCommand {
        actor: actor(),
        command: CredentialReferenceCreateCommandCommand::CredentialReferenceCreate,
        expected_revision: Revision(0),
        payload: CredentialReferenceCreatePayload {
            credential_reference_id: CredentialReferenceId(id("crd", 1)),
            display_name: "Credential Safe".to_owned(),
            provider_id: "provider-safe".to_owned(),
            vault_locator: "fixture://SENSITIVE_LOCATOR_NEVER_PUBLIC".to_owned(),
        },
        request_id: RequestId(id("req", 3)),
        schema_version: SchemaVersion::WinwincodeV1,
        scope: authority_scope.clone(),
    };

    {
        let mut storage = SqliteStorage::open(&root).expect("open storage");
        ProviderCatalogService::new(&mut storage)
            .upsert(&provider_request, &descriptor, at())
            .expect("catalog change");
        ModelSettingsService::new(&mut storage)
            .update(&settings_request, settings.clone(), at())
            .expect("settings change");
        CredentialReferenceService::new(&mut storage)
            .create(&credential, 1_977_725_600_000)
            .expect("credential change");

        ProviderCatalogService::new(&mut storage)
            .upsert(&provider_request, &descriptor, at())
            .expect("catalog replay");
        ModelSettingsService::new(&mut storage)
            .update(&settings_request, settings, at())
            .expect("settings replay");
        CredentialReferenceService::new(&mut storage)
            .create(&credential, 1_977_725_600_000)
            .expect("credential replay");
    }

    let storage = SqliteStorage::open(&root).expect("restart storage");
    let invalidations = public_invalidations(&storage);
    assert_eq!(invalidations.len(), 3);
    let mut sources = invalidations
        .iter()
        .map(|(event, _)| event.source.clone())
        .collect::<Vec<_>>();
    sources.sort_by_key(|source| format!("{source:?}"));
    assert_eq!(
        sources,
        [
            ControlPlaneWebSocketModelRouteAvailabilityInvalidationSource::CredentialReference,
            ControlPlaneWebSocketModelRouteAvailabilityInvalidationSource::ProviderCatalog,
            ControlPlaneWebSocketModelRouteAvailabilityInvalidationSource::Settings,
        ]
    );
    for (event, event_scope) in invalidations {
        assert_eq!(event.source_revision, Revision(1));
        assert_eq!(
            event_scope,
            PublicEventScope::Organization {
                organization_id: OrganizationId(id("org", 1)),
            }
        );
        let encoded = serde_json::to_string(&event).expect("encode invalidation");
        assert!(!encoded.contains("SENSITIVE_LOCATOR_NEVER_PUBLIC"));
        assert!(!encoded.contains("credentialReferenceId"));
        assert!(!encoded.contains("capacity"));
        assert!(!encoded.contains("queue"));
    }
    drop(storage);
    fs::remove_dir_all(root).expect("remove fixture");
}
