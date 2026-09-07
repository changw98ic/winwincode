// SPDX-License-Identifier: Apache-2.0

//! Day-use Provider onboarding: one-time write-only credential input,
//! connection testing, and auditable credential rotation.
//!
//! This module is the interactive counterpart of the startup local model
//! authority ([`crate::model_authority`]): instead of environment-pinned
//! configuration applied at boot, it exposes the application-layer functions
//! a future DSH or `StrongFlow` settings route calls when a user connects a
//! Provider. The user pastes an API key once, the connection is tested, and a
//! model route is established only after the test verified real capability
//! data. No internal identity is ever hand-typed: requests carry a preset id
//! or a custom endpoint plus the secret, and the Credential reference id,
//! request ids, and vault locators are derived here.
//!
//! # Plaintext lifetime
//!
//! The user secret enters as a [`winwincode_control_plane::ResolvedSecret`]
//! (redacted `Debug`, no `Clone`, no `Serialize`, zeroized on drop). It is
//! tracked in a [`winwincode_control_plane::CredentialLeakGate`] fingerprint
//! set and then moved once into the secret store, which consumes and clears
//! the buffer on both success and failure. It is never cloned, serialized,
//! logged, persisted, or returned; every output value is inspected by the
//! tracked gate before it leaves the module, so an accidental copy fails
//! closed. The vault locator handed to the reference metadata is itself
//! write-only: the durable Credential reference aggregate keeps only the
//! locator-free identity, exactly like the startup path.
//!
//! # Ordering contract
//!
//! 1. [`ProviderOnboardingService::create_credential_reference`] commits the
//!    secret-free reference metadata, publishes the secret as immutable
//!    version one, and on any failure deletes the half-created reference and
//!    scrubs any stored material, so nothing half-created survives.
//! 2. [`ProviderOnboardingService::test_connection`] probes the endpoint with
//!    the referenced credential through the [`ConnectionProbe`] port and
//!    reports only probe-confirmed capabilities; everything unconfirmed stays
//!    unknown and is never fabricated from a preset.
//! 3. [`ProviderOnboardingService::establish_model_route`] requires a
//!    [`ConnectionTestReport`] — a value only `test_connection` can
//!    construct, with no public constructor — so the Provider catalog
//!    descriptor and the default route can only ever be built from verified
//!    connection data.
//! 4. [`ProviderOnboardingService::rotate_credential`] probes the replacement
//!    secret before staging its next immutable version beside the current one,
//!    commits the auditable metadata rotation, and only afterwards scrubs the
//!    obsolete version. On every earlier failure the previous credential stays
//!    authoritative, so a route never points at nothing.
//!
//! HTTP routes and UI are intentionally out of scope: this module defines the
//! functions a future route layer calls.

use std::fmt;
use std::io::Read as _;
use std::time::Duration;

use winwincode_api::generated::{
    Actor, CredentialReferenceCreateCommand, CredentialReferenceCreateCommandCommand,
    CredentialReferenceCreatePayload, CredentialReferenceDeleteCommand,
    CredentialReferenceDeleteCommandCommand, CredentialReferenceDeletePayload,
    CredentialReferenceGetParameters, CredentialReferenceGetQuery,
    CredentialReferenceGetQueryQuery, CredentialReferenceProjection,
    CredentialReferenceRotateCommand, CredentialReferenceRotateCommandCommand,
    CredentialReferenceRotatePayload, ModelRoute, OrganizationScope, PageRequest, SchemaVersion,
    Scope,
};
use winwincode_control_plane::{
    CredentialLeakError, CredentialLeakGate, CredentialOutputBoundary, CredentialReferenceError,
    CredentialReferenceErrorKind, CredentialReferenceResolution, CredentialReferenceService,
    CredentialSecretResolutionError, LocalSecretStoreAdapter, ModelCapability,
    ModelCapabilityOrigin, ModelCapabilitySnapshot, ModelCapabilitySource, ModelCatalogModelEntry,
    ModelSelection, ModelSettingsError, ModelSettingsErrorKind, ModelSettingsRequest,
    ModelSettingsService, ModelSettingsTarget, ModelSettingsValues, ProviderCatalogError,
    ProviderCatalogErrorKind, ProviderCatalogRequest, ProviderCatalogService, ProviderDescriptor,
    ProviderPresetsError, ProviderPresetsErrorKind, ResolvedSecret, SecretStoreError,
    SecretStoreErrorKind, SecretStorePort, find_provider_preset, resolve_provider_endpoint,
};
use winwincode_domain::{CredentialReferenceId, Revision};
use winwincode_storage::SqliteStorage;

use crate::model_authority::{derived_request_id, now_millis};
use crate::{StandaloneApplicationClock, SystemStandaloneApplicationClock};

/// Request-id derivation domain of this module, disjoint from the startup
/// local model authority domain by construction.
const PROVIDER_ONBOARDING_DOMAIN: &str = "winwincode.server.provider-onboarding.v1";
/// Adapter for custom endpoints: they are `OpenAI`-compatible by contract,
/// the same adapter the built-in presets route through.
const CUSTOM_ENDPOINT_ADAPTER_KIND: &str = "openai-responses";
/// Same model-count bound as the durable Provider catalog descriptor.
const MAX_CONFIRMED_MODELS: usize = 500;
/// Upper bound for one live connection probe.
const PROBE_TOTAL_TIMEOUT: Duration = Duration::from_secs(15);
/// The probe response body is discarded; it is read only to release the
/// connection, never trusted, and bounded so a hostile endpoint cannot
/// exhaust memory.
const MAX_PROBE_BODY_BYTES: u64 = 64 * 1024;

/// Stable failure categories for Provider onboarding.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderOnboardingErrorKind {
    /// A request input violated the frozen bounds.
    InvalidRequest,
    /// The Provider is not among the presets and no valid custom endpoint
    /// was supplied.
    ProviderNotFound,
    /// The referenced credential is missing, foreign, revoked, or deleted.
    UnknownCredential,
    /// The connection probe did not confirm the endpoint.
    ConnectionFailed,
    /// Route establishment was attempted without a verified connection
    /// report or without any probe-confirmed model capability.
    ConnectionTestRequired,
    /// The probe reported a model capability that violates the catalog
    /// rules; the report is rejected instead of being repaired.
    ProbeReportInvalid,
    /// The credential identity entropy source is unavailable.
    IdentityEntropy,
    /// A secret store failure, with its stable category.
    SecretStore(SecretStoreErrorKind),
    /// A Credential reference lifecycle failure, with its stable category.
    CredentialReference(CredentialReferenceErrorKind),
    /// A Provider catalog failure, with its stable category.
    ProviderCatalog(ProviderCatalogErrorKind),
    /// A model settings failure, with its stable category.
    ModelSettings(ModelSettingsErrorKind),
    /// An output was rejected by the Credential leak gate.
    CredentialLeak,
}

/// Bounded onboarding error that never copies a secret, endpoint, or
/// provider response.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderOnboardingError {
    kind: ProviderOnboardingErrorKind,
    message: &'static str,
}

impl ProviderOnboardingError {
    const fn new(kind: ProviderOnboardingErrorKind, message: &'static str) -> Self {
        Self { kind, message }
    }

    const fn invalid() -> Self {
        Self::new(
            ProviderOnboardingErrorKind::InvalidRequest,
            "Provider onboarding request is invalid",
        )
    }

    const fn connection_test_required() -> Self {
        Self::new(
            ProviderOnboardingErrorKind::ConnectionTestRequired,
            "Provider onboarding requires a verified connection test with at \
             least one confirmed model capability",
        )
    }

    /// Returns the stable machine-readable failure category.
    #[must_use]
    pub const fn kind(&self) -> ProviderOnboardingErrorKind {
        self.kind
    }

    /// Returns the stable secret-free diagnostic message.
    #[must_use]
    pub const fn message(&self) -> &'static str {
        self.message
    }
}

impl fmt::Display for ProviderOnboardingError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message)
    }
}

impl std::error::Error for ProviderOnboardingError {}

impl From<CredentialLeakError> for ProviderOnboardingError {
    fn from(_error: CredentialLeakError) -> Self {
        Self::new(
            ProviderOnboardingErrorKind::CredentialLeak,
            "Provider onboarding output was rejected by the Credential leak gate",
        )
    }
}

impl From<ProviderPresetsError> for ProviderOnboardingError {
    fn from(error: ProviderPresetsError) -> Self {
        match error.kind() {
            ProviderPresetsErrorKind::ProviderNotFound => Self::new(
                ProviderOnboardingErrorKind::ProviderNotFound,
                "Provider was not found among the presets",
            ),
            ProviderPresetsErrorKind::InvalidRequest => Self::invalid(),
            ProviderPresetsErrorKind::CredentialLeak => Self::new(
                ProviderOnboardingErrorKind::CredentialLeak,
                "Provider onboarding output was rejected by the Credential leak gate",
            ),
        }
    }
}

impl From<SecretStoreError> for ProviderOnboardingError {
    fn from(error: SecretStoreError) -> Self {
        Self::new(
            ProviderOnboardingErrorKind::SecretStore(error.kind()),
            "Provider onboarding credential secret operation failed",
        )
    }
}

impl From<CredentialReferenceError> for ProviderOnboardingError {
    fn from(error: CredentialReferenceError) -> Self {
        Self::new(
            ProviderOnboardingErrorKind::CredentialReference(error.kind()),
            "Provider onboarding credential reference operation failed",
        )
    }
}

impl From<CredentialSecretResolutionError> for ProviderOnboardingError {
    fn from(error: CredentialSecretResolutionError) -> Self {
        match error {
            CredentialSecretResolutionError::Reference(reference) => reference.into(),
            CredentialSecretResolutionError::SecretStore(store) => store.into(),
        }
    }
}

impl From<ProviderCatalogError> for ProviderOnboardingError {
    fn from(error: ProviderCatalogError) -> Self {
        Self::new(
            ProviderOnboardingErrorKind::ProviderCatalog(error.kind()),
            "Provider onboarding catalog operation failed",
        )
    }
}

impl From<ModelSettingsError> for ProviderOnboardingError {
    fn from(error: ModelSettingsError) -> Self {
        Self::new(
            ProviderOnboardingErrorKind::ModelSettings(error.kind()),
            "Provider onboarding model settings operation failed",
        )
    }
}

/// Outcome of one connection probe.
///
/// A probe reports what the endpoint confirmed and nothing else: `models`
/// carries only catalog-shaped capabilities the probe itself verified, and an
/// unreachable or rejected endpoint is always [`ProbeOutcome::unreachable`].
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ProbeOutcome {
    succeeded: bool,
    models: Vec<ModelCapability>,
}

impl ProbeOutcome {
    /// The endpoint answered and authenticated; `models` lists the
    /// capabilities the probe verified (possibly none).
    #[must_use]
    pub fn connected(models: Vec<ModelCapability>) -> Self {
        Self {
            succeeded: true,
            models,
        }
    }

    /// The endpoint was unreachable, rejected the credential, or answered
    /// with an unexpected response.
    #[must_use]
    pub fn unreachable() -> Self {
        Self {
            succeeded: false,
            models: Vec::new(),
        }
    }

    /// Whether the endpoint answered and authenticated.
    #[must_use]
    pub const fn succeeded(&self) -> bool {
        self.succeeded
    }

    /// The probe-verified catalog-shaped capabilities.
    #[must_use]
    pub fn confirmed_models(&self) -> &[ModelCapability] {
        &self.models
    }
}

/// Port performing the live connection probe of one endpoint.
///
/// Implementations receive the resolved secret only at the provider-call
/// boundary and must never log, persist, or return it. Unit tests fake this
/// port; the production implementation is [`HttpsConnectionProbe`].
pub trait ConnectionProbe: Send + Sync {
    /// Probes one endpoint with the referenced credential.
    fn probe(&self, provider_id: &str, endpoint: &str, credential: &ResolvedSecret)
    -> ProbeOutcome;
}

/// Production probe: one authenticated `GET {endpoint}/models` request.
///
/// The probe confirms reachability and authentication only. It never derives
/// model capability values from the response — capability data must come
/// from an authoritative source — so a bare `/models` listing leaves every
/// capability unknown and route establishment reports the missing
/// verification instead of fabricating values.
#[derive(Debug)]
pub struct HttpsConnectionProbe {
    agent: ureq::Agent,
}

impl HttpsConnectionProbe {
    /// Builds the probe agent: no redirects (so the credential header is
    /// never replayed to another origin) and a bounded total time.
    #[must_use]
    pub fn new() -> Self {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .max_redirects(0)
            .timeout_global(Some(PROBE_TOTAL_TIMEOUT))
            .build()
            .into();
        Self { agent }
    }
}

impl Default for HttpsConnectionProbe {
    fn default() -> Self {
        Self::new()
    }
}

impl ConnectionProbe for HttpsConnectionProbe {
    fn probe(
        &self,
        _provider_id: &str,
        endpoint: &str,
        credential: &ResolvedSecret,
    ) -> ProbeOutcome {
        // Endpoints are canonical-HTTPS validated before a probe is called;
        // this guard is defense in depth for direct port users.
        if !endpoint.starts_with("https://") {
            return ProbeOutcome::unreachable();
        }
        let Ok(credential_text) = std::str::from_utf8(credential.expose()) else {
            return ProbeOutcome::unreachable();
        };
        // The header is the transport form of the credential at the
        // provider-call boundary; it is dropped right after the call and is
        // never logged or persisted.
        let authorization = format!("Bearer {credential_text}");
        let url = format!("{}/models", endpoint.trim_end_matches('/'));
        let call = self
            .agent
            .get(&url)
            .header("Authorization", authorization.as_str())
            .call();
        drop(authorization);
        let Ok(response) = call else {
            return ProbeOutcome::unreachable();
        };
        if response.status() != 200 {
            return ProbeOutcome::unreachable();
        }
        let mut discarded = String::new();
        let _ = response
            .into_body()
            .into_reader()
            .take(MAX_PROBE_BODY_BYTES)
            .read_to_string(&mut discarded);
        ProbeOutcome::connected(Vec::new())
    }
}

/// Write side of the credential secret store used by onboarding.
///
/// It extends the canonical read-only [`SecretStorePort`] with the immutable
/// version operations the onboarding flows need. The local adapter is the
/// production implementation; tests may fake the port.
pub trait OnboardingSecretStore: SecretStorePort {
    /// Atomically publishes the secret as the version the reference
    /// currently resolves to. Consumes and clears the secret buffer on
    /// success and failure.
    ///
    /// # Errors
    ///
    /// Returns only stable secret-safe categories.
    fn store(
        &self,
        reference: &CredentialReferenceResolution,
        secret: ResolvedSecret,
    ) -> Result<(), SecretStoreError>;

    /// Atomically stages the next immutable version while the current one
    /// stays authoritative.
    ///
    /// # Errors
    ///
    /// Returns only stable secret-safe categories.
    fn stage_rotation(
        &self,
        current: &CredentialReferenceResolution,
        secret: ResolvedSecret,
    ) -> Result<(), SecretStoreError>;

    /// Removes every version except the one the given resolution selects.
    ///
    /// # Errors
    ///
    /// Returns only stable secret-safe categories.
    fn cleanup(&self, current: &CredentialReferenceResolution) -> Result<(), SecretStoreError>;

    /// Removes every stored version of the reference.
    ///
    /// # Errors
    ///
    /// Returns only stable secret-safe categories.
    fn delete(&self, reference: &CredentialReferenceResolution) -> Result<(), SecretStoreError>;
}

impl OnboardingSecretStore for LocalSecretStoreAdapter {
    fn store(
        &self,
        reference: &CredentialReferenceResolution,
        secret: ResolvedSecret,
    ) -> Result<(), SecretStoreError> {
        LocalSecretStoreAdapter::store(self, reference, secret).map(|_| ())
    }

    fn stage_rotation(
        &self,
        current: &CredentialReferenceResolution,
        secret: ResolvedSecret,
    ) -> Result<(), SecretStoreError> {
        LocalSecretStoreAdapter::rotate(self, current, secret).map(|_| ())
    }

    fn cleanup(&self, current: &CredentialReferenceResolution) -> Result<(), SecretStoreError> {
        LocalSecretStoreAdapter::cleanup(self, current).map(|_| ())
    }

    fn delete(&self, reference: &CredentialReferenceResolution) -> Result<(), SecretStoreError> {
        LocalSecretStoreAdapter::delete(self, reference).map(|_| ())
    }
}

/// One credential onboarding request: preset id or custom endpoint plus the
/// one-time secret. The Credential reference id is derived, never supplied.
#[derive(Debug)]
pub struct CreateCredentialRequest {
    /// Who performs the onboarding; recorded in the reference audit trail.
    pub actor: Actor,
    /// Organization scope that will own the credential reference.
    pub organization_scope: OrganizationScope,
    /// Preset Provider identifier, or any stable custom Provider identity
    /// when `custom_endpoint` is supplied.
    pub provider_id: String,
    /// Validated `https` custom endpoint; when absent, the endpoint comes
    /// from the preset.
    pub custom_endpoint: Option<String>,
    /// The one-time write-only user secret.
    pub secret: ResolvedSecret,
}

/// The derived result of one successful credential creation.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CredentialReferenceOnboarded {
    /// The derived internal identity; callers pass it to test and rotate,
    /// users never type it.
    pub credential_reference_id: CredentialReferenceId,
    pub provider_id: String,
    /// The validated endpoint this credential was onboarded for.
    pub endpoint: String,
    pub revision: Revision,
}

/// One connection test request.
#[derive(Debug)]
pub struct TestConnectionRequest {
    pub actor: Actor,
    pub organization_scope: OrganizationScope,
    /// Preset Provider identifier, or a custom Provider identity when
    /// `custom_endpoint` is supplied.
    pub provider_id: String,
    pub custom_endpoint: Option<String>,
    /// The credential reference derived by a previous create.
    pub credential_reference_id: CredentialReferenceId,
}

/// What one connection test verified.
///
/// Only [`ProviderOnboardingService::test_connection`] constructs this value
/// and its fields are private, so route establishment is structurally gated
/// on a real probe result.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ConnectionTestReport {
    provider_id: String,
    endpoint: String,
    credential_reference_id: CredentialReferenceId,
    connection_succeeded: bool,
    /// Probe-verified, catalog-shaped capabilities. Empty means the probe
    /// confirmed nothing.
    confirmed_models: Vec<ModelCapability>,
    /// The preset-joined model view: documented preset models carry a null
    /// capability until the probe confirmed them; unknown means unknown.
    models: Vec<ModelCatalogModelEntry>,
}

impl ConnectionTestReport {
    /// The tested Provider identity.
    #[must_use]
    pub fn provider_id(&self) -> &str {
        &self.provider_id
    }

    /// The tested endpoint.
    #[must_use]
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// The credential reference the probe authenticated with.
    #[must_use]
    pub const fn credential_reference_id(&self) -> &CredentialReferenceId {
        &self.credential_reference_id
    }

    /// Whether the endpoint answered and authenticated.
    #[must_use]
    pub const fn connection_succeeded(&self) -> bool {
        self.connection_succeeded
    }

    /// The probe-verified capabilities, catalog-shaped.
    #[must_use]
    pub fn confirmed_models(&self) -> &[ModelCapability] {
        &self.confirmed_models
    }

    /// The preset-joined model view with honest unknown capabilities.
    #[must_use]
    pub fn models(&self) -> &[ModelCatalogModelEntry] {
        &self.models
    }
}

impl ModelCapabilitySource for ConnectionTestReport {
    fn capability(&self, provider_id: &str, model_id: &str) -> Option<ModelCapabilitySnapshot> {
        if provider_id != self.provider_id {
            return None;
        }
        self.models
            .iter()
            .find(|model| model.model_id == model_id)
            .and_then(|model| model.capability)
    }
}

/// One credential rotation request.
#[derive(Debug)]
pub struct RotateCredentialRequest {
    pub actor: Actor,
    pub organization_scope: OrganizationScope,
    /// The credential reference derived by a previous create.
    pub credential_reference_id: CredentialReferenceId,
    /// The validated custom endpoint used by the credential, when it is not
    /// a built-in Provider preset. A custom endpoint must be supplied again
    /// because it is deliberately not persisted in Credential metadata.
    pub custom_endpoint: Option<String>,
    /// The replacement secret; consumed and cleared by the secret store.
    pub next_secret: ResolvedSecret,
}

/// Durable result of one auditable rotation.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CredentialRotated {
    pub credential_reference_id: CredentialReferenceId,
    /// Rotation version this rotation started from.
    pub previous_rotation_version: u64,
    /// Rotation version that is now authoritative.
    pub rotation_version: u64,
    /// Whether the obsolete version material was scrubbed. A `false` value
    /// means the rotation committed but cleanup should be retried; the old
    /// version is inert either way because the metadata no longer selects it.
    pub cleanup_completed: bool,
}

/// Route establishment inputs; the verified report is the gate.
#[derive(Debug)]
pub struct EstablishRouteRequest {
    pub actor: Actor,
    pub organization_scope: OrganizationScope,
    /// Model to select as the default route. When absent, the confirmed
    /// set must contain exactly one model.
    pub default_model_id: Option<String>,
}

/// Durable result of one verified route establishment.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelRouteEstablished {
    pub provider_id: String,
    pub model_id: String,
    pub credential_reference_id: CredentialReferenceId,
    pub catalog_version: u64,
    pub settings_revision: u64,
}

/// One end-to-end onboarding request: preset id or custom endpoint, the
/// desired default model, and the one-time secret.
#[derive(Debug)]
pub struct OnboardProviderRequest {
    pub actor: Actor,
    pub organization_scope: OrganizationScope,
    pub provider_id: String,
    pub custom_endpoint: Option<String>,
    pub secret: ResolvedSecret,
    pub default_model_id: Option<String>,
}

/// Durable result of one end-to-end onboarding.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProviderOnboarded {
    pub credential_reference: CredentialReferenceOnboarded,
    pub route: ModelRouteEstablished,
}

/// Day-use Provider onboarding service over one storage connection, one
/// secret store, and one connection probe.
pub struct ProviderOnboardingService<'a> {
    storage: &'a mut SqliteStorage,
    secret_store: &'a dyn OnboardingSecretStore,
    probe: &'a dyn ConnectionProbe,
}

impl<'a> ProviderOnboardingService<'a> {
    /// Builds the service over the caller's storage connection.
    #[must_use]
    pub fn new(
        storage: &'a mut SqliteStorage,
        secret_store: &'a dyn OnboardingSecretStore,
        probe: &'a dyn ConnectionProbe,
    ) -> Self {
        Self {
            storage,
            secret_store,
            probe,
        }
    }

    /// Creates a Credential reference from a one-time write-only secret.
    ///
    /// The reference identity, request id, display name, and vault locator
    /// are all derived here. The metadata commits first, the secret is
    /// published as immutable version one second; when either the resolution
    /// or the secret publication fails, the half-created reference is deleted
    /// (tombstone) and any stored material is scrubbed, so the durable state
    /// keeps no half-created reference.
    ///
    /// # Errors
    ///
    /// Rejects an invalid Provider/endpoint combination, a failing secret
    /// store, and reference lifecycle failures; on every failure nothing
    /// half-created remains.
    pub fn create_credential_reference(
        &mut self,
        request: CreateCredentialRequest,
    ) -> Result<CredentialReferenceOnboarded, ProviderOnboardingError> {
        let endpoint = resolve_endpoint(&request.provider_id, request.custom_endpoint.as_deref())?;
        let mut gate = CredentialLeakGate::new();
        gate.track_secret(&request.secret);
        let credential_reference_id = mint_credential_reference_id()?;
        let organization_scope = request.organization_scope.clone();
        let scope = organization(&organization_scope);
        let display_name = credential_display_name(&request.provider_id);
        let locator = format!(
            "local-production://provider-onboarding/{}",
            credential_reference_id.0
        );
        let command = onboarding_create_command(
            &request.actor,
            &organization_scope,
            &credential_reference_id,
            &request.provider_id,
            &display_name,
            &locator,
        )?;
        // Metadata first: the write-only locator participates in the request
        // replay identity but is discarded before any durable state, event,
        // audit, or response is built. The secret store needs the metadata
        // minted resolution, and the failure path below removes the reference
        // again when the secret publication does not complete.
        let created =
            CredentialReferenceService::new(&mut *self.storage).create(&command, now_millis())?;
        let cleanup_revision = created.result.revision.clone();
        let resolution = match CredentialReferenceService::new(&mut *self.storage)
            .resolve(&scope, &credential_reference_id)
        {
            Ok(resolution) => resolution,
            Err(error) => {
                self.cleanup_failed_reference(
                    &request.actor,
                    &organization_scope,
                    &credential_reference_id,
                    cleanup_revision,
                );
                return Err(error.into());
            }
        };
        if let Err(error) = self.secret_store.store(&resolution, request.secret) {
            self.cleanup_failed_reference(
                &request.actor,
                &organization_scope,
                &credential_reference_id,
                cleanup_revision,
            );
            return Err(error.into());
        }
        let onboarded = CredentialReferenceOnboarded {
            credential_reference_id,
            provider_id: request.provider_id.clone(),
            endpoint,
            revision: cleanup_revision,
        };
        gate.inspect_serializable(CredentialOutputBoundary::Serialization, &onboarded)?;
        Ok(onboarded)
    }

    /// Tests the endpoint connection with the referenced credential.
    ///
    /// The reference scope and revocation state are checked before the
    /// secret store is touched, the probe runs through the [`ConnectionProbe`]
    /// port, and the report carries only probe-verified capabilities:
    /// preset models the probe did not confirm are listed with unknown
    /// capability, never with invented values.
    ///
    /// # Errors
    ///
    /// Rejects an invalid Provider/endpoint combination, an unknown or
    /// revoked reference, a failing secret store, and a probe report that
    /// violates the catalog capability rules.
    pub fn test_connection(
        &mut self,
        request: &TestConnectionRequest,
    ) -> Result<ConnectionTestReport, ProviderOnboardingError> {
        let endpoint = resolve_endpoint(&request.provider_id, request.custom_endpoint.as_deref())?;
        let scope = organization(&request.organization_scope);
        let mut gate = CredentialLeakGate::new();
        let secret_store: &dyn SecretStorePort = self.secret_store;
        let secret = CredentialReferenceService::new(&mut *self.storage).resolve_secret(
            secret_store,
            &scope,
            &request.credential_reference_id,
        )?;
        gate.track_secret(&secret);
        let outcome = self.probe.probe(&request.provider_id, &endpoint, &secret);
        drop(secret);
        let confirmed = validate_probe_models(outcome.confirmed_models())?;
        let report = build_report(
            &request.provider_id,
            &endpoint,
            &request.credential_reference_id,
            outcome.succeeded(),
            confirmed,
        );
        gate.inspect_serializable(CredentialOutputBoundary::Serialization, &report)?;
        Ok(report)
    }

    /// Establishes the Provider catalog descriptor and the default model
    /// route from a verified connection report.
    ///
    /// The report has no public constructor, so a route can only ever be
    /// established after a real probe verified the endpoint; a report
    /// without confirmed capabilities is rejected instead of fabricating a
    /// descriptor. The credential reference is re-checked against its
    /// immutable Provider binding before anything is written.
    ///
    /// # Errors
    ///
    /// Rejects an unverified report, an ambiguous or unconfirmed default
    /// model, a reference bound to another Provider, and catalog or settings
    /// failures.
    pub fn establish_model_route(
        &mut self,
        connection: &ConnectionTestReport,
        request: &EstablishRouteRequest,
    ) -> Result<ModelRouteEstablished, ProviderOnboardingError> {
        if !connection.connection_succeeded() || connection.confirmed_models().is_empty() {
            return Err(ProviderOnboardingError::connection_test_required());
        }
        let default_model_id = if let Some(model_id) = request.default_model_id.as_deref() {
            if !connection
                .confirmed_models()
                .iter()
                .any(|model| model.model_id == model_id)
            {
                return Err(ProviderOnboardingError::invalid());
            }
            model_id.to_owned()
        } else if let [only] = connection.confirmed_models() {
            only.model_id.clone()
        } else {
            return Err(ProviderOnboardingError::invalid());
        };
        let scope = organization(&request.organization_scope);
        let resolution = CredentialReferenceService::new(&mut *self.storage)
            .resolve(&scope, connection.credential_reference_id())?;
        if resolution.provider_id() != connection.provider_id() {
            // A credential reference binds its Provider immutably; a stale
            // report must never register a descriptor against the wrong one.
            return Err(ProviderOnboardingError::invalid());
        }
        let descriptor = verified_provider_descriptor(connection);
        let route = ModelRoute {
            provider_id: descriptor.provider_id.clone(),
            model_id: default_model_id,
            credential_reference_id: descriptor.credential_reference_id.clone(),
        };
        let catalog = ProviderCatalogService::new(&mut *self.storage).project(&scope)?;
        let request_id = derived_request_id(
            PROVIDER_ONBOARDING_DOMAIN,
            "provider-catalog-upsert",
            &(
                request.actor.clone(),
                request.organization_scope.clone(),
                // Only the desired state participates; the optimistic
                // catalog version is read from current durable state.
                &descriptor,
            ),
        )
        .map_err(|_| ProviderOnboardingError::invalid())?;
        let receipt = ProviderCatalogService::new(&mut *self.storage).upsert(
            &ProviderCatalogRequest {
                actor: request.actor.clone(),
                scope: scope.clone(),
                request_id,
                expected_catalog_version: catalog.catalog_version,
            },
            &descriptor,
            SystemStandaloneApplicationClock.now_instant(),
        )?;
        let settings_revision = match converge_default_route(
            &mut *self.storage,
            &request.actor,
            &request.organization_scope,
            &route,
        ) {
            Ok(revision) => revision,
            Err(error) => {
                // Catalog and settings are separate durable streams. If the
                // second write fails, disable the descriptor before returning
                // so cleanup by the end-to-end caller cannot leave an active
                // route pointing at a deleted credential reference.
                if let Ok(request_id) = derived_request_id(
                    PROVIDER_ONBOARDING_DOMAIN,
                    "provider-catalog-disable-after-settings-failure",
                    &(
                        request.actor.clone(),
                        request.organization_scope.clone(),
                        descriptor.provider_id.clone(),
                        receipt.catalog_version,
                    ),
                ) {
                    let _ = ProviderCatalogService::new(&mut *self.storage).disable(
                        &ProviderCatalogRequest {
                            actor: request.actor.clone(),
                            scope: scope.clone(),
                            request_id,
                            expected_catalog_version: receipt.catalog_version,
                        },
                        &descriptor.provider_id,
                        SystemStandaloneApplicationClock.now_instant(),
                    );
                }
                return Err(error);
            }
        };
        let established = ModelRouteEstablished {
            provider_id: route.provider_id.clone(),
            model_id: route.model_id.clone(),
            credential_reference_id: route.credential_reference_id.clone(),
            catalog_version: receipt.catalog_version,
            settings_revision,
        };
        CredentialLeakGate::default()
            .inspect_serializable(CredentialOutputBoundary::Serialization, &established)?;
        Ok(established)
    }

    /// Rotates the credential to a new secret, atomically.
    ///
    /// The replacement is staged as the next immutable secret version while
    /// the current version stays authoritative, then the auditable metadata
    /// rotation commits, and only afterwards the obsolete version material is
    /// scrubbed. If staging or the metadata commit fails, the metadata still
    /// selects the previous version, so every existing route keeps
    /// authenticating; the staged material is dropped again when the metadata
    /// did not move.
    ///
    /// # Errors
    ///
    /// Rejects an unknown or revoked reference, a failing secret store, and
    /// reference lifecycle failures; the replacement is probed before any
    /// secret-store write, and on every failure the previous credential stays
    /// authoritative.
    pub fn rotate_credential(
        &mut self,
        request: RotateCredentialRequest,
    ) -> Result<CredentialRotated, ProviderOnboardingError> {
        let scope = organization(&request.organization_scope);
        let mut gate = CredentialLeakGate::new();
        gate.track_secret(&request.next_secret);
        let current = CredentialReferenceService::new(&mut *self.storage)
            .resolve(&scope, &request.credential_reference_id)?;
        let previous_rotation_version = current.rotation_version();
        let endpoint = resolve_endpoint(current.provider_id(), request.custom_endpoint.as_deref())?;
        let outcome = self
            .probe
            .probe(current.provider_id(), &endpoint, &request.next_secret);
        if !outcome.succeeded() {
            return Err(ProviderOnboardingError::new(
                ProviderOnboardingErrorKind::ConnectionFailed,
                "Provider credential rotation probe did not authenticate the endpoint",
            ));
        }
        // Stage the next immutable version while the current one stays
        // authoritative; the secret buffer is consumed and cleared either
        // way. On a staging failure this flow staged nothing of its own, and
        // the previous credential stays authoritative.
        self.secret_store
            .stage_rotation(&current, request.next_secret)
            .map_err(ProviderOnboardingError::from)?;
        let projection = credential_projection(
            &mut *self.storage,
            &request.actor,
            &request.organization_scope,
            &request.credential_reference_id,
        )?;
        let next_version = previous_rotation_version.checked_add(1).ok_or_else(|| {
            ProviderOnboardingError::new(
                ProviderOnboardingErrorKind::InvalidRequest,
                "Provider onboarding rotation version is exhausted",
            )
        })?;
        let locator = format!(
            "local-production://provider-onboarding/{}/v{next_version}",
            request.credential_reference_id.0
        );
        let command = onboarding_rotate_command(
            &request.actor,
            &request.organization_scope,
            &request.credential_reference_id,
            projection.revision.clone(),
            &locator,
        )?;
        match CredentialReferenceService::new(&mut *self.storage).rotate(&command, now_millis()) {
            Ok(response) => {
                let rotation_version =
                    u64::try_from(response.result.rotation_version).map_err(|_| {
                        ProviderOnboardingError::new(
                            ProviderOnboardingErrorKind::CredentialReference(
                                CredentialReferenceErrorKind::InvalidRequest,
                            ),
                            "Provider onboarding credential reference operation failed",
                        )
                    })?;
                // The metadata now selects the new version; scrubbing the
                // obsolete material is the last step and is recoverable.
                let cleanup_completed = match CredentialReferenceService::new(&mut *self.storage)
                    .resolve(&scope, &request.credential_reference_id)
                {
                    Ok(rotated) => self.secret_store.cleanup(&rotated).is_ok(),
                    Err(_) => false,
                };
                let rotated = CredentialRotated {
                    credential_reference_id: request.credential_reference_id.clone(),
                    previous_rotation_version,
                    rotation_version,
                    cleanup_completed,
                };
                gate.inspect_serializable(CredentialOutputBoundary::Serialization, &rotated)?;
                Ok(rotated)
            }
            Err(error) => {
                // Scrub the staged version only when the metadata still
                // selects the version this rotation started from; a
                // concurrent rotation that already advanced the metadata owns
                // the newer versions.
                if let Ok(still_current) = CredentialReferenceService::new(&mut *self.storage)
                    .resolve(&scope, &request.credential_reference_id)
                    && still_current.rotation_version() == previous_rotation_version
                {
                    let _ = self.secret_store.cleanup(&current);
                }
                Err(error.into())
            }
        }
    }

    /// One-shot onboarding: create the credential from the one-time secret,
    /// test the connection, and establish the route only after the test
    /// verified at least one model capability.
    ///
    /// When any step fails — including a failed connection test — the
    /// half-created credential reference is deleted and its stored material
    /// scrubbed, and no route is written.
    ///
    /// # Errors
    ///
    /// Rejects invalid input, a failed or unverified connection test, a
    /// failing secret store, and lifecycle/catalog/settings failures; on
    /// every failure no half-created reference or route remains.
    pub fn onboard_provider(
        &mut self,
        request: OnboardProviderRequest,
    ) -> Result<ProviderOnboarded, ProviderOnboardingError> {
        let credential = self.create_credential_reference(CreateCredentialRequest {
            actor: request.actor.clone(),
            organization_scope: request.organization_scope.clone(),
            provider_id: request.provider_id.clone(),
            custom_endpoint: request.custom_endpoint.clone(),
            secret: request.secret,
        })?;
        let connection = self.test_connection(&TestConnectionRequest {
            actor: request.actor.clone(),
            organization_scope: request.organization_scope.clone(),
            provider_id: request.provider_id.clone(),
            custom_endpoint: request.custom_endpoint,
            credential_reference_id: credential.credential_reference_id.clone(),
        });
        let connection = match connection {
            Ok(report)
                if report.connection_succeeded() && !report.confirmed_models().is_empty() =>
            {
                report
            }
            Ok(_) => {
                self.cleanup_failed_reference(
                    &request.actor,
                    &request.organization_scope,
                    &credential.credential_reference_id,
                    credential.revision.clone(),
                );
                return Err(ProviderOnboardingError::new(
                    ProviderOnboardingErrorKind::ConnectionFailed,
                    "Provider connection test did not verify the endpoint and capabilities",
                ));
            }
            Err(error) => {
                self.cleanup_failed_reference(
                    &request.actor,
                    &request.organization_scope,
                    &credential.credential_reference_id,
                    credential.revision.clone(),
                );
                return Err(error);
            }
        };
        let route = match self.establish_model_route(
            &connection,
            &EstablishRouteRequest {
                actor: request.actor.clone(),
                organization_scope: request.organization_scope.clone(),
                default_model_id: request.default_model_id,
            },
        ) {
            Ok(route) => route,
            Err(error) => {
                self.cleanup_failed_reference(
                    &request.actor,
                    &request.organization_scope,
                    &credential.credential_reference_id,
                    credential.revision.clone(),
                );
                return Err(error);
            }
        };
        Ok(ProviderOnboarded {
            credential_reference: credential,
            route,
        })
    }

    /// Best-effort failure cleanup: scrub stored material, then tombstone
    /// the reference metadata. Idempotent; failures here never mask the
    /// original error.
    fn cleanup_failed_reference(
        &mut self,
        actor: &Actor,
        organization_scope: &OrganizationScope,
        credential_reference_id: &CredentialReferenceId,
        revision: Revision,
    ) {
        let scope = organization(organization_scope);
        if let Ok(resolution) = CredentialReferenceService::new(&mut *self.storage)
            .resolve(&scope, credential_reference_id)
        {
            let _ = self.secret_store.delete(&resolution);
        }
        if let Ok(command) =
            onboarding_delete_command(actor, organization_scope, credential_reference_id, revision)
        {
            let _ =
                CredentialReferenceService::new(&mut *self.storage).delete(&command, now_millis());
        }
    }
}

/// Converges the durable default model route onto the verified selection and
/// returns the resulting settings revision.
fn converge_default_route(
    storage: &mut SqliteStorage,
    actor: &Actor,
    organization_scope: &OrganizationScope,
    route: &ModelRoute,
) -> Result<u64, ProviderOnboardingError> {
    let target = ModelSettingsTarget::Organization {
        scope: organization_scope.clone(),
    };
    let desired_selection = Some(ModelSelection {
        provider_id: route.provider_id.clone(),
        model_id: route.model_id.clone(),
    });
    let stored = ModelSettingsService::new(storage).stored_configuration(&target)?;
    let mut revision = stored.revision;
    if stored.selection != desired_selection || stored.worker_concurrency_limit != 1 {
        let request_id = derived_request_id(
            PROVIDER_ONBOARDING_DOMAIN,
            "model-settings-update",
            &(actor.clone(), organization_scope.clone(), route, 1_u64),
        )
        .map_err(|_| ProviderOnboardingError::invalid())?;
        ModelSettingsService::new(storage).update(
            &ModelSettingsRequest {
                actor: actor.clone(),
                target,
                request_id,
                expected_revision: stored.revision,
            },
            ModelSettingsValues {
                default_model_route: Some(route.clone()),
                worker_concurrency_limit: 1,
            },
            SystemStandaloneApplicationClock.now_instant(),
        )?;
        revision = stored.revision + 1;
    }
    Ok(revision)
}

/// Reads the secret-free reference projection for revision-bound commands.
fn credential_projection(
    storage: &mut SqliteStorage,
    actor: &Actor,
    organization_scope: &OrganizationScope,
    credential_reference_id: &CredentialReferenceId,
) -> Result<CredentialReferenceProjection, ProviderOnboardingError> {
    let request_id = derived_request_id(
        PROVIDER_ONBOARDING_DOMAIN,
        "credential-reference-projection",
        &(
            actor.clone(),
            organization_scope.clone(),
            credential_reference_id.clone(),
        ),
    )
    .map_err(|_| ProviderOnboardingError::invalid())?;
    let query = CredentialReferenceGetQuery {
        actor: actor.clone(),
        page: PageRequest {
            cursor: None,
            limit: 1,
        },
        parameters: CredentialReferenceGetParameters {
            credential_reference_id: credential_reference_id.clone(),
        },
        query: CredentialReferenceGetQueryQuery::CredentialReferenceGet,
        request_id,
        schema_version: SchemaVersion::WinwincodeV1,
        scope: organization(organization_scope),
    };
    let response = CredentialReferenceService::new(storage).get(&query)?;
    Ok(response.result)
}

/// Resolves the endpoint through the presets module: a preset Provider uses
/// its documented base URL; a custom entry is accepted only after the same
/// canonical HTTPS validation the Provider HTTPS adapter applies.
fn resolve_endpoint(
    provider_id: &str,
    custom_endpoint: Option<&str>,
) -> Result<String, ProviderOnboardingError> {
    let resolved = resolve_provider_endpoint(provider_id, custom_endpoint)?;
    Ok(resolved.endpoint().to_owned())
}

/// Mints one canonical `crd_` + 26 character Crockford identity.
fn mint_credential_reference_id() -> Result<CredentialReferenceId, ProviderOnboardingError> {
    let mut random = [0_u8; 13];
    getrandom::fill(&mut random).map_err(|_| {
        ProviderOnboardingError::new(
            ProviderOnboardingErrorKind::IdentityEntropy,
            "credential reference identity entropy is unavailable",
        )
    })?;
    Ok(CredentialReferenceId(format!(
        "crd_{}",
        crate::runtime::crockford_26(&random)
    )))
}

/// Derives the display name shown in projections: the preset display name
/// when the Provider is a preset, otherwise the caller's Provider identity.
fn credential_display_name(provider_id: &str) -> String {
    let base = find_provider_preset(provider_id)
        .map_or_else(|_| provider_id.to_owned(), |preset| preset.display_name);
    format!("{base} credential")
}

/// Builds the Credential reference create command with a derived request id.
fn onboarding_create_command(
    actor: &Actor,
    organization_scope: &OrganizationScope,
    credential_reference_id: &CredentialReferenceId,
    provider_id: &str,
    display_name: &str,
    locator: &str,
) -> Result<CredentialReferenceCreateCommand, ProviderOnboardingError> {
    let payload = CredentialReferenceCreatePayload {
        credential_reference_id: credential_reference_id.clone(),
        display_name: display_name.to_owned(),
        provider_id: provider_id.to_owned(),
        vault_locator: locator.to_owned(),
    };
    let request_id = derived_request_id(
        PROVIDER_ONBOARDING_DOMAIN,
        "credential-reference-create",
        &(actor.clone(), organization_scope.clone(), &payload),
    )
    .map_err(|_| ProviderOnboardingError::invalid())?;
    Ok(CredentialReferenceCreateCommand {
        actor: actor.clone(),
        command: CredentialReferenceCreateCommandCommand::CredentialReferenceCreate,
        expected_revision: Revision(0),
        request_id,
        payload,
        schema_version: SchemaVersion::WinwincodeV1,
        scope: organization(organization_scope),
    })
}

/// Builds the Credential reference rotate command for one staged version.
fn onboarding_rotate_command(
    actor: &Actor,
    organization_scope: &OrganizationScope,
    credential_reference_id: &CredentialReferenceId,
    expected_revision: Revision,
    locator: &str,
) -> Result<CredentialReferenceRotateCommand, ProviderOnboardingError> {
    let payload = CredentialReferenceRotatePayload {
        credential_reference_id: credential_reference_id.clone(),
        vault_locator: locator.to_owned(),
    };
    let request_id = derived_request_id(
        PROVIDER_ONBOARDING_DOMAIN,
        "credential-reference-rotate",
        &(
            actor.clone(),
            organization_scope.clone(),
            &payload,
            expected_revision.clone(),
        ),
    )
    .map_err(|_| ProviderOnboardingError::invalid())?;
    Ok(CredentialReferenceRotateCommand {
        actor: actor.clone(),
        command: CredentialReferenceRotateCommandCommand::CredentialReferenceRotate,
        expected_revision,
        payload,
        request_id,
        schema_version: SchemaVersion::WinwincodeV1,
        scope: organization(organization_scope),
    })
}

/// Builds the cleanup delete command for a half-created reference.
fn onboarding_delete_command(
    actor: &Actor,
    organization_scope: &OrganizationScope,
    credential_reference_id: &CredentialReferenceId,
    expected_revision: Revision,
) -> Result<CredentialReferenceDeleteCommand, ProviderOnboardingError> {
    let request_id = derived_request_id(
        PROVIDER_ONBOARDING_DOMAIN,
        "credential-reference-cleanup-delete",
        &(
            actor.clone(),
            organization_scope.clone(),
            credential_reference_id,
            expected_revision.clone(),
        ),
    )
    .map_err(|_| ProviderOnboardingError::invalid())?;
    Ok(CredentialReferenceDeleteCommand {
        actor: actor.clone(),
        command: CredentialReferenceDeleteCommandCommand::CredentialReferenceDelete,
        expected_revision,
        payload: CredentialReferenceDeletePayload {
            credential_reference_id: credential_reference_id.clone(),
        },
        request_id,
        schema_version: SchemaVersion::WinwincodeV1,
        scope: organization(organization_scope),
    })
}

/// Validates a probe report against the catalog descriptor rules. Nothing is
/// repaired or defaulted: a report that cannot satisfy the rules exactly is
/// rejected.
fn validate_probe_models(
    models: &[ModelCapability],
) -> Result<Vec<ModelCapability>, ProviderOnboardingError> {
    const REJECTED: ProviderOnboardingError = ProviderOnboardingError::new(
        ProviderOnboardingErrorKind::ProbeReportInvalid,
        "connection probe reported a model capability that violates the catalog rules",
    );
    if models.len() > MAX_CONFIRMED_MODELS {
        return Err(ProviderOnboardingError::new(
            ProviderOnboardingErrorKind::ProbeReportInvalid,
            "connection probe reported too many model capabilities",
        ));
    }
    let mut validated = Vec::with_capacity(models.len());
    for model in models {
        if !canonical_token(&model.model_id, 200)
            || model.display_name.is_empty()
            || model.display_name.chars().count() > 500
            || model.context_window_tokens == 0
            || model.max_output_tokens == 0
            || model.max_output_tokens > model.context_window_tokens
            || model.reasoning_efforts.len() > 16
        {
            return Err(REJECTED);
        }
        let mut efforts = model.reasoning_efforts.clone();
        efforts.sort();
        efforts.dedup();
        if efforts.len() != model.reasoning_efforts.len()
            || efforts.iter().any(|effort| !canonical_token(effort, 64))
        {
            return Err(REJECTED);
        }
        if validated
            .iter()
            .any(|existing: &ModelCapability| existing.model_id == model.model_id)
        {
            return Err(REJECTED);
        }
        validated.push(ModelCapability {
            model_id: model.model_id.clone(),
            display_name: model.display_name.clone(),
            context_window_tokens: model.context_window_tokens,
            max_output_tokens: model.max_output_tokens,
            tool_support: model.tool_support,
            reasoning_efforts: efforts,
        });
    }
    Ok(validated)
}

/// Joins the probe outcome with the preset model list into the honest
/// report: unconfirmed preset models keep an unknown capability.
fn build_report(
    provider_id: &str,
    endpoint: &str,
    credential_reference_id: &CredentialReferenceId,
    succeeded: bool,
    confirmed: Vec<ModelCapability>,
) -> ConnectionTestReport {
    let snapshot = |model: &ModelCapability| ModelCapabilitySnapshot {
        context_window_tokens: model.context_window_tokens,
        max_output_tokens: model.max_output_tokens,
        tool_support: model.tool_support,
        origin: ModelCapabilityOrigin::ConnectionProbe,
    };
    let preset = find_provider_preset(provider_id).ok();
    let mut models = Vec::new();
    if let Some(preset) = &preset {
        for model in &preset.models {
            let capability = confirmed
                .iter()
                .find(|confirmed| confirmed.model_id == model.model_id)
                .map(snapshot);
            models.push(ModelCatalogModelEntry {
                model_id: model.model_id.clone(),
                display_name: model.display_name.clone(),
                capability,
            });
        }
    }
    for model in &confirmed {
        if !models.iter().any(|entry| entry.model_id == model.model_id) {
            models.push(ModelCatalogModelEntry {
                model_id: model.model_id.clone(),
                display_name: model.display_name.clone(),
                capability: Some(snapshot(model)),
            });
        }
    }
    ConnectionTestReport {
        provider_id: provider_id.to_owned(),
        endpoint: endpoint.to_owned(),
        credential_reference_id: credential_reference_id.clone(),
        connection_succeeded: succeeded,
        confirmed_models: confirmed,
        models,
    }
}

/// Pairs the verified report with its credential reference to build the
/// Provider catalog descriptor; only probe-confirmed capabilities enter it.
fn verified_provider_descriptor(connection: &ConnectionTestReport) -> ProviderDescriptor {
    let preset = find_provider_preset(&connection.provider_id).ok();
    let (display_name, adapter_kind) = preset.as_ref().map_or_else(
        || {
            (
                connection.provider_id.clone(),
                CUSTOM_ENDPOINT_ADAPTER_KIND.to_owned(),
            )
        },
        |preset| (preset.display_name.clone(), preset.adapter_kind.clone()),
    );
    ProviderDescriptor {
        provider_id: connection.provider_id.clone(),
        display_name,
        adapter_kind,
        credential_reference_id: connection.credential_reference_id.clone(),
        models: connection.confirmed_models().to_vec(),
    }
}

fn organization(organization_scope: &OrganizationScope) -> Scope {
    Scope::OrganizationScope(organization_scope.clone())
}

/// Same identifier charset as the durable Provider catalog validators.
fn canonical_token(value: &str, max_chars: usize) -> bool {
    !value.is_empty()
        && value.len() <= max_chars
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'/' | b'-')
        })
}
