// SPDX-License-Identifier: Apache-2.0

//! Production OIDC, SAML, and SCIM verifier composition.
//!
//! The Control Plane sends opaque protocol material to one independently
//! operated verification authority over verified HTTPS. The authority owns
//! provider-specific JOSE/XML-DSig parsing and returns only signed claims. A
//! short-lived authority credential is resolved through the canonical
//! Credential reference and `SecretStore` boundary for every request. Raw ID
//! tokens, SAML responses, SCIM bearers, authority credentials, and remote
//! response text are never persisted or copied into public errors.

use std::{fmt, io::Read as _, str, sync::Arc, sync::Mutex, time::Duration};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use winwincode_api::generated::Scope;
use winwincode_domain::CredentialReferenceId;
use winwincode_storage::ProductStateStorage;

use crate::{
    CredentialReferenceService, OidcTokenVerifier, ProtocolVerificationError, SamlResponseVerifier,
    ScimBearerVerifier, SecretStorePort, VerifiedOidcClaims, VerifiedSamlClaims,
    VerifiedScimClient,
};

const REQUEST_SCHEMA: &str = "winwincode.enterprise-identity-verification-request.v1";
const RESPONSE_SCHEMA: &str = "winwincode.enterprise-identity-verification-response.v1";
const MAX_ENDPOINT_BYTES: usize = 2_048;
const MAX_AUTHORITY_CREDENTIAL_BYTES: usize = 16 * 1024;
const MAX_PROTOCOL_CREDENTIAL_BYTES: usize = 1_048_576;
const MAX_RESPONSE_BYTES: usize = 256 * 1024;
const MAX_TLS_ROOT_BYTES: usize = 64 * 1024;
const MAX_TLS_ROOTS: usize = 32;

/// TLS trust roots for the enterprise identity verification authority.
#[derive(Clone)]
pub enum EnterpriseIdentityVerifierTlsRoots {
    /// Mozilla `WebPKI` roots shipped by the pinned HTTP stack.
    WebPki,
    /// An explicit DER trust set for private or deterministic deployments.
    Specific(Vec<Vec<u8>>),
}

impl fmt::Debug for EnterpriseIdentityVerifierTlsRoots {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WebPki => formatter.write_str("EnterpriseIdentityVerifierTlsRoots::WebPki"),
            Self::Specific(roots) => formatter
                .debug_tuple("EnterpriseIdentityVerifierTlsRoots::Specific")
                .field(&roots.len())
                .finish(),
        }
    }
}

/// Bounded deadlines for one remote verification request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EnterpriseIdentityVerifierTimeouts {
    pub connect: Duration,
    pub response: Duration,
    pub total: Duration,
}

/// Verified HTTPS configuration for one protocol verification authority.
#[derive(Clone)]
pub struct EnterpriseIdentityVerifierConfig {
    endpoint: String,
    timeouts: EnterpriseIdentityVerifierTimeouts,
    max_response_bytes: usize,
    tls_roots: EnterpriseIdentityVerifierTlsRoots,
}

impl EnterpriseIdentityVerifierConfig {
    /// Builds a no-proxy, no-redirect, `WebPKI`-verified authority config.
    ///
    /// The endpoint is a base path. Exact `/oidc`, `/saml`, and `/scim`
    /// operation paths are appended by the verifier.
    ///
    /// # Errors
    ///
    /// Rejects non-HTTPS or credential-bearing endpoints, unsafe deadlines,
    /// and unbounded response limits.
    pub fn try_new(
        endpoint: String,
        timeouts: EnterpriseIdentityVerifierTimeouts,
        max_response_bytes: usize,
    ) -> Result<Self, ProtocolVerificationError> {
        let config = Self {
            endpoint,
            timeouts,
            max_response_bytes,
            tls_roots: EnterpriseIdentityVerifierTlsRoots::WebPki,
        };
        config.validate()?;
        Ok(config)
    }

    /// Replaces `WebPKI` roots with an explicit non-empty DER trust set.
    ///
    /// # Errors
    ///
    /// Rejects empty or unbounded trust material.
    pub fn with_specific_tls_roots(
        mut self,
        roots: Vec<Vec<u8>>,
    ) -> Result<Self, ProtocolVerificationError> {
        if roots.is_empty()
            || roots.len() > MAX_TLS_ROOTS
            || roots
                .iter()
                .any(|root| root.is_empty() || root.len() > MAX_TLS_ROOT_BYTES)
        {
            return Err(ProtocolVerificationError::invalid_message());
        }
        self.tls_roots = EnterpriseIdentityVerifierTlsRoots::Specific(roots);
        self.validate()?;
        Ok(self)
    }

    fn validate(&self) -> Result<(), ProtocolVerificationError> {
        if !canonical_https_endpoint(&self.endpoint)
            || self.timeouts.connect.is_zero()
            || self.timeouts.response.is_zero()
            || self.timeouts.total.is_zero()
            || self.timeouts.connect > self.timeouts.total
            || self.timeouts.response > self.timeouts.total
            || self.max_response_bytes == 0
            || self.max_response_bytes > MAX_RESPONSE_BYTES
        {
            return Err(ProtocolVerificationError::invalid_message());
        }
        Ok(())
    }
}

impl fmt::Debug for EnterpriseIdentityVerifierConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EnterpriseIdentityVerifierConfig")
            .field("endpoint", &"[REDACTED]")
            .field("timeouts", &self.timeouts)
            .field("max_response_bytes", &self.max_response_bytes)
            .field("tls_roots", &self.tls_roots)
            .finish()
    }
}

/// Factory for three verifier-port implementations sharing one HTTPS client
/// and one canonical Credential reference source.
pub struct EnterpriseIdentityProductionVerifiers {
    inner: Arc<VerificationAuthorityClient>,
}

impl EnterpriseIdentityProductionVerifiers {
    /// Builds the production verifier set.
    ///
    /// The supplied metadata storage is used only to resolve the current,
    /// non-revoked Credential reference. Secret bytes are loaded from the
    /// supplied `SecretStorePort` immediately before each HTTPS request.
    ///
    /// # Errors
    ///
    /// Rejects malformed endpoint, TLS, timeout, or response-bound settings.
    pub fn try_new(
        config: EnterpriseIdentityVerifierConfig,
        metadata: Box<dyn ProductStateStorage>,
        secrets: Arc<dyn SecretStorePort>,
        scope: Scope,
        credential_reference_id: CredentialReferenceId,
    ) -> Result<Self, ProtocolVerificationError> {
        config.validate()?;
        let root_certs = match &config.tls_roots {
            EnterpriseIdentityVerifierTlsRoots::WebPki => ureq::tls::RootCerts::WebPki,
            EnterpriseIdentityVerifierTlsRoots::Specific(values) => values
                .iter()
                .map(|value| ureq::tls::Certificate::from_der(value).to_owned())
                .collect::<Vec<_>>()
                .into(),
        };
        let tls = ureq::tls::TlsConfig::builder()
            .provider(ureq::tls::TlsProvider::Rustls)
            .root_certs(root_certs)
            .use_sni(true)
            .disable_verification(false)
            .build();
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .max_redirects(0)
            .proxy(None)
            .timeout_connect(Some(config.timeouts.connect))
            .timeout_recv_response(Some(config.timeouts.response))
            .timeout_recv_body(Some(config.timeouts.response))
            .timeout_global(Some(config.timeouts.total))
            .tls_config(tls)
            .build()
            .into();
        Ok(Self {
            inner: Arc::new(VerificationAuthorityClient {
                config,
                agent,
                credential_source: Mutex::new(CredentialSource {
                    metadata,
                    secrets,
                    scope,
                    credential_reference_id,
                }),
            }),
        })
    }

    /// Consumes the factory and returns the three frozen verifier ports.
    #[must_use]
    pub fn into_verifiers(
        self,
    ) -> (
        ProductionOidcTokenVerifier,
        ProductionSamlResponseVerifier,
        ProductionScimBearerVerifier,
    ) {
        (
            ProductionOidcTokenVerifier {
                inner: Arc::clone(&self.inner),
            },
            ProductionSamlResponseVerifier {
                inner: Arc::clone(&self.inner),
            },
            ProductionScimBearerVerifier { inner: self.inner },
        )
    }
}

impl fmt::Debug for EnterpriseIdentityProductionVerifiers {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EnterpriseIdentityProductionVerifiers")
            .field("config", &self.inner.config)
            .field("credential_source", &"[REDACTED]")
            .finish_non_exhaustive()
    }
}

/// Production implementation of the frozen OIDC verifier port.
pub struct ProductionOidcTokenVerifier {
    inner: Arc<VerificationAuthorityClient>,
}

impl OidcTokenVerifier for ProductionOidcTokenVerifier {
    fn verify(&mut self, token: &str) -> Result<VerifiedOidcClaims, ProtocolVerificationError> {
        let response = self.inner.verify(Operation::Oidc, token.as_bytes())?;
        parse_oidc_response(&response)
    }
}

impl fmt::Debug for ProductionOidcTokenVerifier {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProductionOidcTokenVerifier")
            .field("authority", &"[REDACTED]")
            .finish()
    }
}

/// Production implementation of the frozen SAML verifier port.
pub struct ProductionSamlResponseVerifier {
    inner: Arc<VerificationAuthorityClient>,
}

impl SamlResponseVerifier for ProductionSamlResponseVerifier {
    fn verify(&mut self, response: &[u8]) -> Result<VerifiedSamlClaims, ProtocolVerificationError> {
        let response = self.inner.verify(Operation::Saml, response)?;
        parse_saml_response(&response)
    }
}

impl fmt::Debug for ProductionSamlResponseVerifier {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProductionSamlResponseVerifier")
            .field("authority", &"[REDACTED]")
            .finish()
    }
}

/// Production implementation of the frozen SCIM bearer verifier port.
pub struct ProductionScimBearerVerifier {
    inner: Arc<VerificationAuthorityClient>,
}

impl ScimBearerVerifier for ProductionScimBearerVerifier {
    fn verify(&mut self, bearer: &str) -> Result<VerifiedScimClient, ProtocolVerificationError> {
        let response = self.inner.verify(Operation::Scim, bearer.as_bytes())?;
        parse_scim_response(&response)
    }
}

impl fmt::Debug for ProductionScimBearerVerifier {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProductionScimBearerVerifier")
            .field("authority", &"[REDACTED]")
            .finish()
    }
}

struct VerificationAuthorityClient {
    config: EnterpriseIdentityVerifierConfig,
    agent: ureq::Agent,
    credential_source: Mutex<CredentialSource>,
}

struct CredentialSource {
    metadata: Box<dyn ProductStateStorage>,
    secrets: Arc<dyn SecretStorePort>,
    scope: Scope,
    credential_reference_id: CredentialReferenceId,
}

#[derive(Clone, Copy)]
enum Operation {
    Oidc,
    Saml,
    Scim,
}

impl Operation {
    const fn path(self) -> &'static str {
        match self {
            Self::Oidc => "oidc",
            Self::Saml => "saml",
            Self::Scim => "scim",
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::Oidc => "verify_oidc",
            Self::Saml => "verify_saml",
            Self::Scim => "verify_scim",
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct VerificationRequest<'a> {
    schema: &'static str,
    operation: &'static str,
    credential_base64: &'a str,
}

struct SensitiveBytes(Vec<u8>);

impl Drop for SensitiveBytes {
    fn drop(&mut self) {
        self.0.fill(0);
    }
}

impl VerificationAuthorityClient {
    fn verify(
        &self,
        operation: Operation,
        credential: &[u8],
    ) -> Result<SensitiveBytes, ProtocolVerificationError> {
        if credential.is_empty() || credential.len() > MAX_PROTOCOL_CREDENTIAL_BYTES {
            return Err(ProtocolVerificationError::invalid_message());
        }
        let encoded_capacity = credential
            .len()
            .checked_add(2)
            .and_then(|value| value.checked_div(3))
            .and_then(|value| value.checked_mul(4))
            .ok_or_else(ProtocolVerificationError::invalid_message)?;
        let body = {
            let mut encoded = SensitiveBytes(vec![0_u8; encoded_capacity]);
            let encoded_len = STANDARD
                .encode_slice(credential, &mut encoded.0)
                .map_err(|_| ProtocolVerificationError::invalid_message())?;
            encoded.0.truncate(encoded_len);
            let encoded_text = str::from_utf8(&encoded.0)
                .map_err(|_| ProtocolVerificationError::invalid_message())?;
            let request = VerificationRequest {
                schema: REQUEST_SCHEMA,
                operation: operation.label(),
                credential_base64: encoded_text,
            };
            SensitiveBytes(
                serde_json::to_vec(&request)
                    .map_err(|_| ProtocolVerificationError::invalid_message())?,
            )
        };
        if body.0.len() > MAX_PROTOCOL_CREDENTIAL_BYTES.saturating_mul(2) {
            return Err(ProtocolVerificationError::invalid_message());
        }
        self.send(operation, &body.0)
    }

    fn send(
        &self,
        operation: Operation,
        body: &[u8],
    ) -> Result<SensitiveBytes, ProtocolVerificationError> {
        let authority_credential = {
            let mut source = self
                .credential_source
                .lock()
                .map_err(|_| ProtocolVerificationError::key_unavailable())?;
            let CredentialSource {
                metadata,
                secrets,
                scope,
                credential_reference_id,
            } = &mut *source;
            CredentialReferenceService::new(metadata.as_mut())
                .resolve_secret(secrets.as_ref(), scope, credential_reference_id)
                .map_err(|_| ProtocolVerificationError::key_unavailable())?
        };
        if authority_credential.expose().len() > MAX_AUTHORITY_CREDENTIAL_BYTES {
            return Err(ProtocolVerificationError::key_unavailable());
        }
        let mut authorization = SensitiveBytes(Vec::with_capacity(
            "Bearer ".len() + authority_credential.expose().len(),
        ));
        authorization.0.extend_from_slice(b"Bearer ");
        authorization
            .0
            .extend_from_slice(authority_credential.expose());
        let authorization_text = str::from_utf8(&authorization.0)
            .ok()
            .filter(|value| valid_header_value(value))
            .ok_or_else(ProtocolVerificationError::key_unavailable)?;
        let endpoint = format!("{}/{}", self.config.endpoint, operation.path());
        let response = self
            .agent
            .post(&endpoint)
            .header("Accept", "application/json")
            .header("Authorization", authorization_text)
            .header("Content-Type", "application/json")
            .send(body);
        drop(authorization);
        drop(authority_credential);
        let response = response.map_err(|_| ProtocolVerificationError::key_unavailable())?;
        let status = response.status().as_u16();
        if !(200..=299).contains(&status) {
            return Err(status_error(status));
        }
        if !response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .is_some_and(canonical_json_content_type)
        {
            return Err(ProtocolVerificationError::invalid_message());
        }
        let mut reader = response.into_body().into_reader();
        let mut bytes = Vec::new();
        reader
            .by_ref()
            .take(
                u64::try_from(self.config.max_response_bytes)
                    .map_err(|_| ProtocolVerificationError::invalid_message())?
                    + 1,
            )
            .read_to_end(&mut bytes)
            .map_err(|_| ProtocolVerificationError::key_unavailable())?;
        if bytes.is_empty() || bytes.len() > self.config.max_response_bytes {
            bytes.fill(0);
            return Err(ProtocolVerificationError::invalid_message());
        }
        Ok(SensitiveBytes(bytes))
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct VerifiedOidcResponse {
    schema: String,
    operation: String,
    claims: OidcClaimsWire,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct OidcClaimsWire {
    issuer: String,
    audiences: Vec<String>,
    subject: String,
    token_id: String,
    issued_at_millis: u64,
    not_before_millis: u64,
    expires_at_millis: u64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct VerifiedSamlResponse {
    schema: String,
    operation: String,
    claims: SamlClaimsWire,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SamlClaimsWire {
    issuer: String,
    audiences: Vec<String>,
    subject: String,
    assertion_id: String,
    issued_at_millis: u64,
    not_before_millis: u64,
    expires_at_millis: u64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct VerifiedScimResponse {
    schema: String,
    operation: String,
    claims: ScimClaimsWire,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ScimClaimsWire {
    issuer: String,
    audiences: Vec<String>,
    client_id: String,
    expires_at_millis: u64,
}

fn parse_oidc_response(
    bytes: &SensitiveBytes,
) -> Result<VerifiedOidcClaims, ProtocolVerificationError> {
    let response: VerifiedOidcResponse = serde_json::from_slice(&bytes.0)
        .map_err(|_| ProtocolVerificationError::invalid_message())?;
    if response.schema != RESPONSE_SCHEMA || response.operation != Operation::Oidc.label() {
        return Err(ProtocolVerificationError::invalid_message());
    }
    Ok(VerifiedOidcClaims {
        issuer: response.claims.issuer,
        audiences: response.claims.audiences,
        subject: response.claims.subject,
        token_id: response.claims.token_id,
        issued_at_millis: response.claims.issued_at_millis,
        not_before_millis: response.claims.not_before_millis,
        expires_at_millis: response.claims.expires_at_millis,
    })
}

fn parse_saml_response(
    bytes: &SensitiveBytes,
) -> Result<VerifiedSamlClaims, ProtocolVerificationError> {
    let response: VerifiedSamlResponse = serde_json::from_slice(&bytes.0)
        .map_err(|_| ProtocolVerificationError::invalid_message())?;
    if response.schema != RESPONSE_SCHEMA || response.operation != Operation::Saml.label() {
        return Err(ProtocolVerificationError::invalid_message());
    }
    Ok(VerifiedSamlClaims {
        issuer: response.claims.issuer,
        audiences: response.claims.audiences,
        subject: response.claims.subject,
        assertion_id: response.claims.assertion_id,
        issued_at_millis: response.claims.issued_at_millis,
        not_before_millis: response.claims.not_before_millis,
        expires_at_millis: response.claims.expires_at_millis,
    })
}

fn parse_scim_response(
    bytes: &SensitiveBytes,
) -> Result<VerifiedScimClient, ProtocolVerificationError> {
    let response: VerifiedScimResponse = serde_json::from_slice(&bytes.0)
        .map_err(|_| ProtocolVerificationError::invalid_message())?;
    if response.schema != RESPONSE_SCHEMA || response.operation != Operation::Scim.label() {
        return Err(ProtocolVerificationError::invalid_message());
    }
    Ok(VerifiedScimClient {
        issuer: response.claims.issuer,
        audiences: response.claims.audiences,
        client_id: response.claims.client_id,
        expires_at_millis: response.claims.expires_at_millis,
    })
}

fn status_error(status: u16) -> ProtocolVerificationError {
    match status {
        400 | 422 => ProtocolVerificationError::invalid_message(),
        409 => ProtocolVerificationError::signature_rejected(),
        _ => ProtocolVerificationError::key_unavailable(),
    }
}

fn canonical_https_endpoint(value: &str) -> bool {
    value.len() <= MAX_ENDPOINT_BYTES
        && value.starts_with("https://")
        && value.trim() == value
        && !value.ends_with('/')
        && !value.contains(['?', '#'])
        && !value.chars().any(char::is_control)
        && value
            .strip_prefix("https://")
            .and_then(|rest| rest.split('/').next())
            .is_some_and(|authority| !authority.is_empty() && !authority.contains('@'))
}

fn canonical_json_content_type(value: &str) -> bool {
    value.eq_ignore_ascii_case("application/json")
}

fn valid_header_value(value: &str) -> bool {
    value.len() > "Bearer ".len()
        && value.len() <= "Bearer ".len() + MAX_AUTHORITY_CREDENTIAL_BYTES
        && value.trim() == value
        && !value.chars().any(char::is_control)
}
