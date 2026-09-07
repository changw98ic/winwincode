// SPDX-License-Identifier: Apache-2.0

//! Startup-time local model authority configuration.
//!
//! Every boot re-submits the declarative local model configuration (one
//! Credential reference, one Provider catalog descriptor, one default model
//! route) against the durable data directory. The submitted request ids are
//! derived deterministically from the exact desired input instead of being
//! fixed constants, which keeps both halves of the idempotency contract:
//!
//! * an unchanged model-route environment derives the same request ids as the
//!   previous run, so the durable receipts replay and the configuration is
//!   never applied twice; and
//! * a changed model-route environment derives fresh request ids, so the new
//!   desired state applies through the sanctioned lifecycle commands
//!   (Credential create, Provider catalog upsert, model settings update)
//!   instead of reusing a requestId for different input and failing startup
//!   with a request-conflict error.
//!
//! The derivation only covers the desired-state inputs. Optimistic
//! concurrency inputs that the startup path reads from current durable state
//! (the catalog version and the settings revision) are deliberately excluded,
//! because they change independently of the desired configuration.
//!
//! The Provider catalog converges before the settings update so the settings
//! write validates its route against a catalog that already enables the
//! desired model. The stored settings are read without catalog resolution so
//! a startup interrupted between the catalog and settings commits converges
//! on the next boot instead of failing an unresolved read.
//!
//! This startup path is distinct from the runtime replay guard and does not
//! weaken it: the storage receipt contract still rejects any requestId reused
//! with a different command digest, including derived ids within one process.
//! A restart is not a replay — it is a new declarative submission whose
//! request id tracks its input. One deliberate boundary remains: a Credential
//! reference identity binds its Provider immutably, so when the operator pins
//! a credential reference that is recorded for a different Provider, startup
//! fails with an explicit configuration error and a reference for the new
//! Provider must be configured instead.

use std::{env, path::PathBuf};

use sha2::{Digest, Sha256};
use winwincode_api::generated::{
    Actor, CredentialReferenceCreateCommand, CredentialReferenceCreateCommandCommand,
    CredentialReferenceCreatePayload, ModelRoute, OrganizationScope, OrganizationScopeKind, Scope,
};
use winwincode_control_plane::{
    CatalogAvailability, CredentialReferenceErrorKind, CredentialReferenceService,
    LocalSecretStoreAdapter, ModelCapability, ModelSelection, ModelSettingsRequest,
    ModelSettingsService, ModelSettingsTarget, ModelSettingsValues, ModelToolSupport,
    ProviderCatalogRequest, ProviderCatalogService, ProviderDescriptor, ResolvedSecret,
    StructuredOutputSupport,
};
use winwincode_domain::{
    CredentialReferenceId, RepositoryScope, RequestId, Revision, SchemaVersion, UserActor,
    UserActorKind, UserId,
};
use winwincode_storage::SqliteStorage;

use crate::runtime::crockford_26;
use crate::{StandaloneApplicationClock, SystemStandaloneApplicationClock};

/// The local model route configured for this Server process.
#[derive(Clone, Debug)]
pub struct LocalModelRoute {
    /// Provider that executes the local model route.
    pub provider: String,
    /// Model selected within the Provider.
    pub model: String,
    /// Credential reference paying for the Provider route.
    pub credential_reference: CredentialReferenceId,
}

impl LocalModelRoute {
    /// Resolves the model route from the Server environment with loopback
    /// defaults.
    ///
    /// # Errors
    ///
    /// Rejects environment values that are not valid UTF-8.
    pub fn from_environment() -> Result<Self, Box<dyn std::error::Error>> {
        Ok(Self {
            provider: environment_or("WWC_SERVER_MODEL_PROVIDER_ID", "winwincode-loopback")?,
            model: environment_or("WWC_SERVER_MODEL_ID", "loopback-model")?,
            credential_reference: CredentialReferenceId(environment_or(
                "WWC_SERVER_MODEL_CREDENTIAL_REFERENCE_ID",
                "crd_00000000000000000000000001",
            )?),
        })
    }

    /// Returns the generated model route for projections and commands.
    #[must_use]
    pub fn as_api_route(&self) -> ModelRoute {
        ModelRoute {
            provider_id: self.provider.clone(),
            model_id: self.model.clone(),
            credential_reference_id: self.credential_reference.clone(),
        }
    }
}

/// Applies the startup local model authority for the durable first Owner.
///
/// The function is idempotent for an unchanged model route and supersedes the
/// durable route selection when the model-route environment changed since the
/// previous run against the same data directory.
///
/// # Errors
///
/// Rejects storage failures, failures from the underlying lifecycle services,
/// and a pinned Credential reference that is bound to a different Provider.
pub fn configure_local_model_authority(
    storage: &mut SqliteStorage,
    owner_user_id: &UserId,
    repository_scope: &RepositoryScope,
    model_route: &LocalModelRoute,
    secret_directory: PathBuf,
) -> Result<(), Box<dyn std::error::Error>> {
    let occurred_at = SystemStandaloneApplicationClock.now_instant();
    let organization_scope = OrganizationScope {
        kind: OrganizationScopeKind::Organization,
        organization_id: repository_scope.organization_id.clone(),
    };
    ensure_local_credential(storage, owner_user_id, &organization_scope, model_route)?;
    store_local_credential_secret(storage, &organization_scope, model_route, secret_directory)?;
    converge_provider_catalog(
        storage,
        owner_user_id,
        repository_scope,
        &organization_scope,
        model_route,
    )?;
    converge_model_settings(
        storage,
        owner_user_id,
        repository_scope,
        &organization_scope,
        model_route,
        occurred_at,
    )?;
    Ok(())
}

/// Creates or verifies the local model Credential reference.
///
/// # Errors
///
/// Rejects storage failures and a pinned Credential reference that is bound
/// to a different Provider; Credential references cannot be rebound.
fn ensure_local_credential(
    storage: &mut SqliteStorage,
    owner_user_id: &UserId,
    organization_scope: &OrganizationScope,
    model_route: &LocalModelRoute,
) -> Result<(), Box<dyn std::error::Error>> {
    let organization = Scope::OrganizationScope(organization_scope.clone());
    let credential_command =
        credential_create_command(owner_user_id, organization_scope, model_route)?;
    match CredentialReferenceService::new(storage).create(&credential_command, now_millis()) {
        Ok(_) => Ok(()),
        Err(error) if error.kind() == CredentialReferenceErrorKind::WrongState => {
            let current = CredentialReferenceService::new(storage)
                .resolve(&organization, &model_route.credential_reference)?;
            if current.provider_id() == model_route.provider {
                Ok(())
            } else {
                Err(format!(
                    "model credential reference {} is bound to Provider '{}'; credential \
                     references cannot be rebound, so configure \
                     WWC_SERVER_MODEL_CREDENTIAL_REFERENCE_ID with a reference for Provider '{}'",
                    model_route.credential_reference.0,
                    current.provider_id(),
                    model_route.provider
                )
                .into())
            }
        }
        Err(error) => Err(Box::new(error)),
    }
}

/// Publishes the fixed loopback secret for the current reference resolution.
///
/// # Errors
///
/// Rejects secret-store failures and a missing or revoked reference.
fn store_local_credential_secret(
    storage: &mut SqliteStorage,
    organization_scope: &OrganizationScope,
    model_route: &LocalModelRoute,
    secret_directory: PathBuf,
) -> Result<(), Box<dyn std::error::Error>> {
    let organization = Scope::OrganizationScope(organization_scope.clone());
    let credential = CredentialReferenceService::new(storage)
        .resolve(&organization, &model_route.credential_reference)?;
    let secret_store = LocalSecretStoreAdapter::open(secret_directory)?;
    secret_store.store(
        &credential,
        ResolvedSecret::from_bytes(b"winwincode-local-loopback-secret".to_vec())?,
    )?;
    Ok(())
}

/// Converges the Provider catalog descriptor onto the desired model route.
///
/// # Errors
///
/// Rejects catalog storage failures and conflicting scoped request replays.
fn converge_provider_catalog(
    storage: &mut SqliteStorage,
    owner_user_id: &UserId,
    repository_scope: &RepositoryScope,
    organization_scope: &OrganizationScope,
    model_route: &LocalModelRoute,
) -> Result<(), Box<dyn std::error::Error>> {
    let organization = Scope::OrganizationScope(organization_scope.clone());
    let descriptor = local_provider_descriptor(model_route);
    let catalog = ProviderCatalogService::new(storage).project(&organization)?;
    let provider_matches = catalog.providers.iter().any(|provider| {
        provider.provider_id == descriptor.provider_id
            && provider.availability == CatalogAvailability::Enabled
            && provider.credential_reference_id == descriptor.credential_reference_id
            && provider.adapter_kind == descriptor.adapter_kind
            && provider.models.len() == 1
            && provider.models.iter().any(|model| {
                model.model_id == model_route.model
                    && model.availability == CatalogAvailability::Enabled
            })
    });
    if provider_matches {
        return Ok(());
    }
    ProviderCatalogService::new(storage).upsert(
        &ProviderCatalogRequest {
            actor: local_actor(owner_user_id),
            scope: organization,
            request_id: derived_request_id(
                LOCAL_MODEL_AUTHORITY_DOMAIN,
                "provider-catalog-upsert",
                &(
                    owner_user_id.0.clone(),
                    repository_scope.organization_id.0.clone(),
                    descriptor.clone(),
                ),
            )?,
            expected_catalog_version: catalog.catalog_version,
        },
        &descriptor,
    )?;
    Ok(())
}

/// Converges the durable default model route onto the desired route.
///
/// # Errors
///
/// Rejects settings storage failures and conflicting scoped request replays.
fn converge_model_settings(
    storage: &mut SqliteStorage,
    owner_user_id: &UserId,
    repository_scope: &RepositoryScope,
    organization_scope: &OrganizationScope,
    model_route: &LocalModelRoute,
    occurred_at: winwincode_domain::Instant,
) -> Result<(), Box<dyn std::error::Error>> {
    let route = model_route.as_api_route();
    let target = ModelSettingsTarget::Organization {
        scope: organization_scope.clone(),
    };
    let desired_selection = Some(ModelSelection {
        provider_id: model_route.provider.clone(),
        model_id: model_route.model.clone(),
    });
    // The stored settings are read without catalog resolution on purpose:
    // the catalog upsert may have disabled the previously selected model, and
    // a startup interrupted between the catalog and settings commits must
    // still converge on the next boot instead of failing an unresolved read.
    let stored = ModelSettingsService::new(storage).stored_configuration(&target)?;
    if stored.selection == desired_selection && stored.worker_concurrency_limit == 1 {
        return Ok(());
    }
    ModelSettingsService::new(storage).update(
        &ModelSettingsRequest {
            actor: local_actor(owner_user_id),
            target,
            request_id: derived_request_id(
                LOCAL_MODEL_AUTHORITY_DOMAIN,
                "model-settings-update",
                &(
                    owner_user_id.0.clone(),
                    repository_scope.organization_id.0.clone(),
                    route.clone(),
                    // The desired concurrency limit; part of the desired state
                    // so a future constant change derives fresh ids.
                    1_u64,
                ),
            )?,
            expected_revision: stored.revision,
        },
        ModelSettingsValues {
            default_model_route: Some(route),
            worker_concurrency_limit: 1,
        },
        occurred_at,
    )?;
    Ok(())
}

/// The loopback Provider descriptor desired for one model route.
fn local_provider_descriptor(model_route: &LocalModelRoute) -> ProviderDescriptor {
    ProviderDescriptor {
        provider_id: model_route.provider.clone(),
        display_name: "WinWinCode local loopback Provider".to_owned(),
        adapter_kind: "deterministic-loopback".to_owned(),
        credential_reference_id: model_route.credential_reference.clone(),
        models: vec![ModelCapability {
            model_id: model_route.model.clone(),
            display_name: "WinWinCode local loopback model".to_owned(),
            context_window_tokens: 128_000,
            max_output_tokens: 16_000,
            tool_support: ModelToolSupport::Parallel,
            structured_output_support: StructuredOutputSupport::JsonSchemaStrict,
            reasoning_efforts: vec!["high".to_owned(), "medium".to_owned()],
        }],
    }
}

fn local_actor(owner_user_id: &UserId) -> Actor {
    Actor::UserActor(UserActor {
        kind: UserActorKind::User,
        id: owner_user_id.clone(),
    })
}

/// Builds the startup Credential reference create command.
///
/// The requestId is derived from the full desired input (Owner, organization,
/// and payload), so an unchanged environment replays the original receipt
/// while a changed environment naturally derives a fresh id. Exposing the
/// exact construction also lets callers prove the runtime replay guard: the
/// returned command's requestId reused with any different input must be
/// rejected by [`CredentialReferenceService::create`].
///
/// # Errors
///
/// Rejects an input that cannot be encoded for the request id derivation.
pub fn credential_create_command(
    owner_user_id: &UserId,
    organization_scope: &OrganizationScope,
    model_route: &LocalModelRoute,
) -> Result<CredentialReferenceCreateCommand, serde_json::Error> {
    let payload = CredentialReferenceCreatePayload {
        credential_reference_id: model_route.credential_reference.clone(),
        display_name: "WinWinCode local model credential".to_owned(),
        provider_id: model_route.provider.clone(),
        vault_locator: "local-production://loopback".to_owned(),
    };
    Ok(CredentialReferenceCreateCommand {
        actor: Actor::UserActor(UserActor {
            kind: UserActorKind::User,
            id: owner_user_id.clone(),
        }),
        command: CredentialReferenceCreateCommandCommand::CredentialReferenceCreate,
        expected_revision: Revision(0),
        request_id: derived_request_id(
            LOCAL_MODEL_AUTHORITY_DOMAIN,
            "credential-reference-create",
            &(
                owner_user_id.0.clone(),
                organization_scope.organization_id.0.clone(),
                payload.clone(),
            ),
        )?,
        payload,
        schema_version: SchemaVersion::WinwincodeV1,
        scope: Scope::OrganizationScope(organization_scope.clone()),
    })
}

/// Request-id derivation domain of the startup local model authority.
const LOCAL_MODEL_AUTHORITY_DOMAIN: &str = "winwincode.server.local-model-authority.v1";

/// Derives the deterministic requestId for one exact desired input.
///
/// Identical inputs derive identical ids so the durable receipt replays the
/// original result; changed inputs derive different ids so the desired state
/// applies without reusing a requestId for different input. The provider
/// onboarding module reuses this canonical derivation with its own domain
/// string so the two flows can never collide.
pub(crate) fn derived_request_id<T: serde::Serialize>(
    domain: &str,
    purpose: &str,
    input: &T,
) -> Result<RequestId, serde_json::Error> {
    let input = serde_json::to_vec(input)?;
    let mut digest = Sha256::new();
    digest.update(domain.as_bytes());
    digest.update([0]);
    digest.update(purpose.as_bytes());
    digest.update([0]);
    digest.update(u64::try_from(input.len()).unwrap_or(u64::MAX).to_be_bytes());
    digest.update(&input);
    Ok(RequestId(format!(
        "req_{}",
        crockford_26(&digest.finalize())
    )))
}

fn environment_or(name: &str, default: &str) -> Result<String, Box<dyn std::error::Error>> {
    match env::var(name) {
        Ok(value) if !value.is_empty() => Ok(value),
        Ok(_) | Err(env::VarError::NotPresent) => Ok(default.to_owned()),
        Err(error) => Err(error.into()),
    }
}

pub(crate) fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use winwincode_control_plane::CredentialReferenceErrorKind as ErrorKind;

    fn temporary_directory(name: &str) -> PathBuf {
        env::temp_dir().join(format!(
            "winwincode-model-authority-{name}-{}",
            std::process::id()
        ))
    }

    fn organization_scope() -> OrganizationScope {
        OrganizationScope {
            kind: OrganizationScopeKind::Organization,
            organization_id: winwincode_domain::OrganizationId(
                "org_00000000000000000000000001".to_owned(),
            ),
        }
    }

    fn owner() -> UserId {
        UserId("usr_00000000000000000000000001".to_owned())
    }

    fn route(provider: &str, model: &str, credential: &str) -> LocalModelRoute {
        LocalModelRoute {
            provider: provider.to_owned(),
            model: model.to_owned(),
            credential_reference: CredentialReferenceId(credential.to_owned()),
        }
    }

    #[test]
    fn derived_request_ids_are_deterministic_input_sensitive_and_canonical() {
        let owner = owner();
        let scope = organization_scope();
        let first = credential_create_command(
            &owner,
            &scope,
            &route("p1", "m1", "crd_00000000000000000000000001"),
        )
        .expect("command derives");
        let second = credential_create_command(
            &owner,
            &scope,
            &route("p1", "m1", "crd_00000000000000000000000001"),
        )
        .expect("command derives");
        assert_eq!(first.request_id, second.request_id);
        let changed = credential_create_command(
            &owner,
            &scope,
            &route("p2", "m1", "crd_00000000000000000000000001"),
        )
        .expect("command derives");
        assert_ne!(first.request_id, changed.request_id);

        for request_id in [first.request_id.0, changed.request_id.0] {
            let suffix = request_id
                .strip_prefix("req_")
                .expect("derived id keeps the req_ prefix");
            assert_eq!(suffix.len(), 26);
            assert!(suffix.bytes().all(|byte| {
                byte.is_ascii_digit()
                    || matches!(
                        byte,
                        b'A'..=b'H' | b'J' | b'K' | b'M' | b'N' | b'P'..=b'T' | b'V'..=b'Z'
                    )
            }));
        }
    }

    #[test]
    fn reused_startup_request_id_with_different_input_is_still_rejected() {
        let data_directory = temporary_directory("replay-guard");
        let mut storage = SqliteStorage::open(&data_directory).expect("storage opens");
        let owner = owner();
        let scope = organization_scope();
        let model_route = route("p1", "m1", "crd_00000000000000000000000001");
        configure_local_model_authority(
            &mut storage,
            &owner,
            &repository_scope(),
            &model_route,
            temporary_directory("replay-guard-secrets"),
        )
        .expect("first configuration applies");

        // The exact command the startup path submitted, resubmitted within the
        // same run with a different payload under the same requestId: the
        // runtime anti-replay guard must reject it.
        let mut duplicate =
            credential_create_command(&owner, &scope, &model_route).expect("command derives");
        duplicate.payload.display_name = "A different desired credential".to_owned();
        let error = CredentialReferenceService::new(&mut storage)
            .create(&duplicate, now_millis())
            .expect_err("same requestId with different input must be rejected");
        assert_eq!(error.kind(), ErrorKind::RequestConflict);
    }

    fn repository_scope() -> RepositoryScope {
        use winwincode_api::generated::{
            ProjectId, RepositoryId, RepositoryScopeKind, WorkspaceId,
        };
        RepositoryScope {
            kind: RepositoryScopeKind::Repository,
            organization_id: winwincode_domain::OrganizationId(
                "org_00000000000000000000000001".to_owned(),
            ),
            workspace_id: WorkspaceId("wsp_00000000000000000000000001".to_owned()),
            project_id: ProjectId("prj_00000000000000000000000001".to_owned()),
            repository_id: RepositoryId("rep_00000000000000000000000001".to_owned()),
        }
    }
}
