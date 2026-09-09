// SPDX-License-Identifier: Apache-2.0

//! Restart regression coverage for the startup local model authority.
//!
//! The Server must tolerate model-route environment changes across restarts
//! against the same data directory, while a duplicate submission that reuses
//! a requestId with different input inside one run stays rejected.

use std::path::{Path, PathBuf};

use winwincode_api::generated::{Actor, OrganizationScope, OrganizationScopeKind, Scope};
use winwincode_control_plane::{
    CatalogAvailability, CredentialReferenceErrorKind, CredentialReferenceService, ModelCapability,
    ModelSettingsService, ModelSettingsTarget, ModelToolSupport, ProviderCatalogRequest,
    ProviderCatalogService, ProviderDescriptor,
};
use winwincode_domain::{
    CredentialReferenceId, OrganizationId, ProjectId, RepositoryId, RepositoryScope,
    RepositoryScopeKind, RequestId, UserActor, UserActorKind, UserId, WorkspaceId,
};
use winwincode_server::{
    LocalModelRoute, configure_local_model_authority, credential_create_command,
};
use winwincode_storage::SqliteStorage;

const OWNER: &str = "usr_00000000000000000000000001";
const ORGANIZATION: &str = "org_00000000000000000000000001";
const FIRST_CREDENTIAL: &str = "crd_00000000000000000000000001";
const SECOND_CREDENTIAL: &str = "crd_00000000000000000000000002";

fn temporary_directory(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "winwincode-server-model-authority-{name}-{}",
        std::process::id()
    ))
}

fn repository_scope() -> RepositoryScope {
    RepositoryScope {
        kind: RepositoryScopeKind::Repository,
        organization_id: OrganizationId(ORGANIZATION.to_owned()),
        workspace_id: WorkspaceId("wsp_00000000000000000000000001".to_owned()),
        project_id: ProjectId("prj_00000000000000000000000001".to_owned()),
        repository_id: RepositoryId("rep_00000000000000000000000001".to_owned()),
    }
}

fn owner() -> UserId {
    UserId(OWNER.to_owned())
}

fn route(provider: &str, model: &str, credential: &str) -> LocalModelRoute {
    LocalModelRoute {
        provider: provider.to_owned(),
        model: model.to_owned(),
        anthropic_endpoint: None,
        credential_reference: CredentialReferenceId(credential.to_owned()),
    }
}

fn apply(
    data_directory: &Path,
    model_route: &LocalModelRoute,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut storage = SqliteStorage::open(data_directory)?;
    let result = configure_local_model_authority(
        &mut storage,
        &owner(),
        &repository_scope(),
        model_route,
        data_directory.join("secrets"),
    );
    drop(storage);
    result
}

fn organization_scope() -> Scope {
    Scope::OrganizationScope(OrganizationScope {
        kind: OrganizationScopeKind::Organization,
        organization_id: OrganizationId(ORGANIZATION.to_owned()),
    })
}

fn catalog_version(storage: &mut SqliteStorage) -> u64 {
    ProviderCatalogService::new(storage)
        .project(&organization_scope())
        .expect("catalog projects")
        .catalog_version
}

fn enabled_models(storage: &mut SqliteStorage, provider_id: &str) -> Vec<String> {
    let catalog = ProviderCatalogService::new(storage)
        .project(&organization_scope())
        .expect("catalog projects");
    catalog
        .providers
        .iter()
        .filter(|provider| provider.provider_id == provider_id)
        .flat_map(|provider| provider.models.iter())
        .filter(|model| model.availability == CatalogAvailability::Enabled)
        .map(|model| model.model_id.clone())
        .collect()
}

fn stored_route(storage: &mut SqliteStorage) -> Option<winwincode_api::generated::ModelRoute> {
    ModelSettingsService::new(storage)
        .project(&ModelSettingsTarget::Organization {
            scope: OrganizationScope {
                kind: OrganizationScopeKind::Organization,
                organization_id: OrganizationId(ORGANIZATION.to_owned()),
            },
        })
        .expect("settings project")
        .default_model_route
}

#[test]
fn restarts_with_changed_model_route_reach_ready() {
    let data_directory = temporary_directory("restart-matrix");
    let first = route("winwincode-loopback", "loopback-model", FIRST_CREDENTIAL);

    // First run against an empty data directory.
    apply(&data_directory, &first).expect("first run reaches ready");
    let (first_catalog_version, first_settings) = {
        let mut storage = SqliteStorage::open(&data_directory).expect("storage opens");
        (catalog_version(&mut storage), stored_route(&mut storage))
    };
    assert_eq!(
        first_settings
            .as_ref()
            .map(|route| route.provider_id.clone()),
        Some(first.provider.clone())
    );

    // Second run with an unchanged environment: ready, without any
    // re-application of the durable state.
    apply(&data_directory, &first).expect("unchanged restart reaches ready");
    {
        let mut storage = SqliteStorage::open(&data_directory).expect("storage opens");
        assert_eq!(catalog_version(&mut storage), first_catalog_version);
        assert_eq!(stored_route(&mut storage), first_settings);
    }

    // Third run with a changed model id: ready, and the desired route wins.
    let second = route("winwincode-loopback", "second-model", FIRST_CREDENTIAL);
    apply(&data_directory, &second).expect("model change restart reaches ready");
    {
        let mut storage = SqliteStorage::open(&data_directory).expect("storage opens");
        assert_eq!(
            enabled_models(&mut storage, "winwincode-loopback"),
            vec!["second-model".to_owned()]
        );
        assert_eq!(
            stored_route(&mut storage)
                .as_ref()
                .map(|route| route.model_id.clone()),
            Some("second-model".to_owned())
        );
    }

    // Fourth run with a coherent provider/model/credential change: ready,
    // and the new credential reference is created and selected.
    let third = route("second-provider", "third-model", SECOND_CREDENTIAL);
    apply(&data_directory, &third).expect("provider change restart reaches ready");
    {
        let mut storage = SqliteStorage::open(&data_directory).expect("storage opens");
        let credential = CredentialReferenceService::new(&mut storage)
            .resolve(
                &organization_scope(),
                &CredentialReferenceId(SECOND_CREDENTIAL.to_owned()),
            )
            .expect("changed credential reference exists");
        assert_eq!(credential.provider_id(), "second-provider");
        assert_eq!(
            enabled_models(&mut storage, "second-provider"),
            vec!["third-model".to_owned()]
        );
        let route = stored_route(&mut storage).expect("settings keep a default route");
        assert_eq!(route.provider_id, "second-provider");
        assert_eq!(route.model_id, "third-model");
        assert_eq!(route.credential_reference_id.0, SECOND_CREDENTIAL);
    }

    let _ = std::fs::remove_dir_all(&data_directory);
}

#[test]
fn interrupted_startup_between_catalog_and_settings_converges() {
    let data_directory = temporary_directory("interrupted");
    let first = route("winwincode-loopback", "loopback-model", FIRST_CREDENTIAL);
    apply(&data_directory, &first).expect("first run reaches ready");

    // Simulate a startup that committed the catalog descriptor for the new
    // model and then stopped before converging the settings selection: the
    // stored selection now points at a model the catalog disabled.
    {
        let mut storage = SqliteStorage::open(&data_directory).expect("storage opens");
        let expected_catalog_version = catalog_version(&mut storage);
        ProviderCatalogService::new(&mut storage)
            .upsert(
                &ProviderCatalogRequest {
                    actor: Actor::UserActor(UserActor {
                        kind: UserActorKind::User,
                        id: owner(),
                    }),
                    scope: organization_scope(),
                    request_id: RequestId("req_00000000000000000000000090".to_owned()),
                    expected_catalog_version,
                },
                &ProviderDescriptor {
                    provider_id: "winwincode-loopback".to_owned(),
                    display_name: "WinWinCode local loopback Provider".to_owned(),
                    adapter_kind: "deterministic-loopback".to_owned(),
                    credential_reference_id: CredentialReferenceId(FIRST_CREDENTIAL.to_owned()),
                    models: vec![ModelCapability {
                        model_id: "second-model".to_owned(),
                        display_name: "WinWinCode local loopback model".to_owned(),
                        context_window_tokens: 128_000,
                        max_output_tokens: 16_000,
                        tool_support: ModelToolSupport::Parallel,
                        structured_output_support:
                            winwincode_control_plane::StructuredOutputSupport::JsonSchemaStrict,
                        reasoning_efforts: vec!["high".to_owned(), "medium".to_owned()],
                    }],
                },
            )
            .expect("interrupted catalog upsert applies");
    }

    let second = route("winwincode-loopback", "second-model", FIRST_CREDENTIAL);
    apply(&data_directory, &second).expect("interrupted startup converges on the next boot");
    {
        let mut storage = SqliteStorage::open(&data_directory).expect("storage opens");
        assert_eq!(
            stored_route(&mut storage)
                .expect("settings keep a default route")
                .model_id,
            "second-model"
        );
    }

    let _ = std::fs::remove_dir_all(&data_directory);
}

#[test]
fn pinned_credential_reference_bound_to_another_provider_is_a_configuration_error() {
    let data_directory = temporary_directory("provider-binding");
    apply(
        &data_directory,
        &route("winwincode-loopback", "loopback-model", FIRST_CREDENTIAL),
    )
    .expect("first run reaches ready");

    let error = apply(
        &data_directory,
        &route("second-provider", "second-model", FIRST_CREDENTIAL),
    )
    .expect_err("a pinned reference cannot serve a different provider");
    let message = error.to_string();
    assert!(
        message.contains("cannot be rebound"),
        "unexpected startup error: {message}"
    );
    assert!(
        !message.contains("reused with different input"),
        "the request id guard must not surface here: {message}"
    );

    let _ = std::fs::remove_dir_all(&data_directory);
}

#[test]
fn duplicate_submission_within_a_run_is_still_rejected() {
    let data_directory = temporary_directory("within-run-replay");
    let model_route = route("winwincode-loopback", "loopback-model", FIRST_CREDENTIAL);
    let mut storage = SqliteStorage::open(&data_directory).expect("storage opens");
    configure_local_model_authority(
        &mut storage,
        &owner(),
        &repository_scope(),
        &model_route,
        data_directory.join("secrets"),
    )
    .expect("configuration applies");

    // The exact command the startup path submitted, resubmitted in the same
    // run under the same requestId with a different payload: the durable
    // anti-replay guard must reject it.
    let mut duplicate = credential_create_command(
        &owner(),
        &OrganizationScope {
            kind: OrganizationScopeKind::Organization,
            organization_id: OrganizationId(ORGANIZATION.to_owned()),
        },
        &model_route,
    )
    .expect("the startup command derives");
    duplicate.payload.provider_id = "second-provider".to_owned();
    let error = CredentialReferenceService::new(&mut storage)
        .create(&duplicate, 1_700_000_000_000)
        .expect_err("same requestId with different input must be rejected");
    assert_eq!(error.kind(), CredentialReferenceErrorKind::RequestConflict);

    drop(storage);
    let _ = std::fs::remove_dir_all(&data_directory);
}

#[test]
fn external_route_validates_https_and_never_selects_loopback() {
    let mut external = route("zhipu-glm", "glm-test", "crd_00000000000000000000000001");
    external.anthropic_endpoint =
        Some("https://open.bigmodel.cn/api/anthropic/v1/messages".to_owned());
    assert!(matches!(
        external.provider_config().expect("external config"),
        winwincode_control_plane::StandaloneProviderConfig::HttpsSse(_)
    ));
    for endpoint in [
        "http://example.com/messages",
        "https://secret@example.com/messages",
        "",
    ] {
        external.anthropic_endpoint = Some(endpoint.to_owned());
        assert!(external.provider_config().is_err());
    }
}
