// SPDX-License-Identifier: Apache-2.0

//! Provider-neutral signed webhook mapping and HTTPS delivery adapter.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::str::FromStr;

use hmac::{Hmac, Mac};
use serde_json::Value;
use sha2::{Digest, Sha256};
use winwincode_audit::AuditScope;
use winwincode_domain::{CredentialReferenceId, EnterpriseIntegrationId, Sha256Digest};

use crate::model::{MAX_SAFE_INTEGER, validate_integration_id};
use crate::{
    ConnectorAuthority, ConnectorCallError, ConnectorCallErrorKind, ConnectorPort,
    InboundNormalizationContext, InboundWebhookMetadata, InboundWebhookRequest, IntegrationError,
    IntegrationErrorKind, NormalizedInboundEvent, OutboundCallReceipt, OutboundClaim,
    SignatureVerificationError, WebhookSignatureVerifier,
};

/// Canonical protocol identifier for provider-neutral webhook connectors.
pub const WEBHOOK_CONNECTOR_PROTOCOL: &str = "webhook.v1";

const MAX_HMAC_SECRET_BYTES: usize = 4_096;
const MAX_MAPPING_FIELDS: usize = 128;
const MAX_MAPPING_TEMPLATES: usize = 128;
const MAX_ENDPOINT_BYTES: usize = 2_048;
const MAX_ALLOWED_HOSTS: usize = 64;
const MAX_RESOLVED_ADDRESSES: usize = 16;
const ABSOLUTE_MAX_BODY_BYTES: usize = 1_048_576;
const ABSOLUTE_MAX_RESPONSE_BYTES: usize = 2_097_152;

/// Supported generic webhook authentication boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WebhookAuthenticationMode {
    HmacSha256,
    MutualTls,
}

/// Bounded raw-body and network limits applied before protocol mapping or I/O.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WebhookLimits {
    inbound_body_bytes: usize,
    outbound_body_bytes: usize,
    response_body_bytes: usize,
    timeout_millis: u64,
}

impl WebhookLimits {
    /// Builds closed webhook size and timeout limits.
    ///
    /// # Errors
    ///
    /// Rejects zero or values beyond the framework's absolute resource bounds.
    pub fn try_new(
        inbound_body_bytes: usize,
        outbound_body_bytes: usize,
        response_body_bytes: usize,
        timeout_millis: u64,
    ) -> Result<Self, IntegrationError> {
        if inbound_body_bytes == 0
            || inbound_body_bytes > ABSOLUTE_MAX_BODY_BYTES
            || outbound_body_bytes == 0
            || outbound_body_bytes > ABSOLUTE_MAX_BODY_BYTES
            || response_body_bytes == 0
            || response_body_bytes > ABSOLUTE_MAX_RESPONSE_BYTES
            || timeout_millis == 0
            || timeout_millis > MAX_SAFE_INTEGER
        {
            return Err(invalid());
        }
        Ok(Self {
            inbound_body_bytes,
            outbound_body_bytes,
            response_body_bytes,
            timeout_millis,
        })
    }

    #[must_use]
    pub const fn max_inbound_body_bytes(self) -> usize {
        self.inbound_body_bytes
    }

    #[must_use]
    pub const fn max_outbound_body_bytes(self) -> usize {
        self.outbound_body_bytes
    }

    #[must_use]
    pub const fn max_response_body_bytes(self) -> usize {
        self.response_body_bytes
    }

    #[must_use]
    pub const fn request_timeout_millis(self) -> u64 {
        self.timeout_millis
    }
}

impl Default for WebhookLimits {
    fn default() -> Self {
        Self {
            inbound_body_bytes: 256 * 1_024,
            outbound_body_bytes: 256 * 1_024,
            response_body_bytes: 256 * 1_024,
            timeout_millis: 30_000,
        }
    }
}

/// Timestamp validation applied before a credential verifier is called.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WebhookInboundPolicy {
    authentication: WebhookAuthenticationMode,
    maximum_age_millis: u64,
    maximum_future_skew_millis: u64,
}

impl WebhookInboundPolicy {
    /// Builds a bounded signed-request time window.
    ///
    /// # Errors
    ///
    /// Rejects zero, unsafe, or future-skew values wider than the accepted age.
    pub fn try_new(
        authentication: WebhookAuthenticationMode,
        maximum_age_millis: u64,
        maximum_future_skew_millis: u64,
    ) -> Result<Self, IntegrationError> {
        if maximum_age_millis == 0
            || maximum_age_millis > MAX_SAFE_INTEGER
            || maximum_future_skew_millis > maximum_age_millis
        {
            return Err(invalid());
        }
        Ok(Self {
            authentication,
            maximum_age_millis,
            maximum_future_skew_millis,
        })
    }

    #[must_use]
    pub const fn authentication(self) -> WebhookAuthenticationMode {
        self.authentication
    }
}

/// One exact HTTPS destination plus an explicit hostname allowlist.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WebhookEndpoint {
    url: String,
    host: String,
    port: u16,
}

impl WebhookEndpoint {
    /// Validates a credential-free HTTPS endpoint against exact allowed hosts.
    ///
    /// # Errors
    ///
    /// Rejects HTTP, user information, queries, fragments, private literal
    /// addresses, wildcard hosts, or a host outside the allowlist.
    pub fn try_new(
        value: impl Into<String>,
        allowed_hosts: impl IntoIterator<Item = String>,
    ) -> Result<Self, IntegrationError> {
        let value = value.into();
        if value.is_empty() || value.len() > MAX_ENDPOINT_BYTES {
            return Err(invalid());
        }
        let uri = ureq::http::Uri::from_str(&value).map_err(|_| invalid())?;
        let authority = uri.authority().ok_or_else(invalid)?;
        let host = uri.host().ok_or_else(invalid)?.to_ascii_lowercase();
        let port = uri.port_u16().unwrap_or(443);
        if uri.scheme_str() != Some("https")
            || authority.as_str().contains('@')
            || uri
                .path_and_query()
                .is_some_and(|value| value.query().is_some())
            || !valid_hostname(&host)
            || port == 0
        {
            return Err(invalid());
        }
        if let Ok(address) = host.parse::<IpAddr>()
            && !public_address(address)
        {
            return Err(invalid());
        }
        let allowed = allowed_hosts
            .into_iter()
            .map(|value| value.to_ascii_lowercase())
            .collect::<BTreeSet<_>>();
        if allowed.is_empty()
            || allowed.len() > MAX_ALLOWED_HOSTS
            || allowed.iter().any(|value| !valid_hostname(value))
            || !allowed.contains(&host)
        {
            return Err(invalid());
        }
        Ok(Self {
            url: value,
            host,
            port,
        })
    }

    #[must_use]
    pub fn url(&self) -> &str {
        &self.url
    }

    #[must_use]
    pub fn host(&self) -> &str {
        &self.host
    }

    #[must_use]
    pub const fn port(&self) -> u16 {
        self.port
    }
}

/// One JSON-pointer to canonical output-field mapping.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WebhookMappingField {
    output_name: String,
    input_pointer: String,
    required: bool,
}

impl WebhookMappingField {
    /// Builds a required mapping field.
    ///
    /// # Errors
    ///
    /// Rejects invalid output names or JSON pointers.
    pub fn required(
        output_name: impl Into<String>,
        input_pointer: impl Into<String>,
    ) -> Result<Self, IntegrationError> {
        Self::try_new(output_name.into(), input_pointer.into(), true)
    }

    /// Builds an optional mapping field.
    ///
    /// # Errors
    ///
    /// Rejects invalid output names or JSON pointers.
    pub fn optional(
        output_name: impl Into<String>,
        input_pointer: impl Into<String>,
    ) -> Result<Self, IntegrationError> {
        Self::try_new(output_name.into(), input_pointer.into(), false)
    }

    fn try_new(
        output_name: String,
        input_pointer: String,
        required: bool,
    ) -> Result<Self, IntegrationError> {
        if !valid_output_name(&output_name) || !valid_json_pointer(&input_pointer) {
            return Err(invalid());
        }
        Ok(Self {
            output_name,
            input_pointer,
            required,
        })
    }
}

/// Versioned, deterministic provider-payload mapping into one formal command.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WebhookMappingTemplate {
    version: u16,
    event_type: String,
    command_name: String,
    fields: Vec<WebhookMappingField>,
}

impl WebhookMappingTemplate {
    /// Builds a canonical mapping template.
    ///
    /// # Errors
    ///
    /// Rejects unsupported versions, duplicate outputs, invalid names, or an
    /// empty/oversized mapping.
    pub fn try_new(
        version: u16,
        event_type: impl Into<String>,
        command_name: impl Into<String>,
        mut fields: Vec<WebhookMappingField>,
    ) -> Result<Self, IntegrationError> {
        let event_type = event_type.into();
        let command_name = command_name.into();
        if version != 1
            || !valid_portable_name(&event_type)
            || !valid_portable_name(&command_name)
            || fields.is_empty()
            || fields.len() > MAX_MAPPING_FIELDS
        {
            return Err(invalid());
        }
        fields.sort_by(|left, right| left.output_name.cmp(&right.output_name));
        if fields
            .windows(2)
            .any(|pair| pair[0].output_name == pair[1].output_name)
        {
            return Err(invalid());
        }
        Ok(Self {
            version,
            event_type,
            command_name,
            fields,
        })
    }

    #[must_use]
    pub const fn version(&self) -> u16 {
        self.version
    }

    #[must_use]
    pub fn event_type(&self) -> &str {
        &self.event_type
    }

    #[must_use]
    pub fn command_name(&self) -> &str {
        &self.command_name
    }

    fn map(&self, payload: &[u8]) -> Result<NormalizedInboundEvent, ConnectorCallError> {
        let value: Value = serde_json::from_slice(payload)
            .map_err(|_| permanent_error("WEBHOOK_PAYLOAD_INVALID"))?;
        let mut mapped = BTreeMap::new();
        for field in &self.fields {
            match value.pointer(&field.input_pointer) {
                Some(value) => {
                    mapped.insert(field.output_name.clone(), value.clone());
                }
                None if field.required => {
                    return Err(permanent_error("WEBHOOK_MAPPING_REQUIRED_FIELD_MISSING"));
                }
                None => {}
            }
        }
        let bytes =
            serde_json::to_vec(&mapped).map_err(|_| permanent_error("WEBHOOK_MAPPING_INVALID"))?;
        NormalizedInboundEvent::try_new(self.command_name.clone(), bytes)
            .map_err(|_| permanent_error("WEBHOOK_MAPPING_INVALID"))
    }
}

/// Secret-bearing HMAC key returned only to the signature authority.
pub struct WebhookHmacSecret(Vec<u8>);

impl WebhookHmacSecret {
    /// Builds a bounded HMAC secret.
    ///
    /// # Errors
    ///
    /// Rejects keys shorter than 256 bits or beyond the bounded secret size.
    pub fn try_new(value: impl Into<Vec<u8>>) -> Result<Self, IntegrationError> {
        let value = value.into();
        if value.len() < 32 || value.len() > MAX_HMAC_SECRET_BYTES {
            return Err(invalid());
        }
        Ok(Self(value))
    }
}

impl fmt::Debug for WebhookHmacSecret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("WebhookHmacSecret([REDACTED])")
    }
}

impl Drop for WebhookHmacSecret {
    fn drop(&mut self) {
        self.0.fill(0);
    }
}

/// Credential resolution failure category for a generic webhook.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WebhookCredentialErrorKind {
    Rejected,
    Revoked,
    Unavailable,
}

/// Secret-safe webhook credential failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WebhookCredentialError {
    kind: WebhookCredentialErrorKind,
}

impl WebhookCredentialError {
    #[must_use]
    pub const fn rejected() -> Self {
        Self {
            kind: WebhookCredentialErrorKind::Rejected,
        }
    }

    #[must_use]
    pub const fn revoked() -> Self {
        Self {
            kind: WebhookCredentialErrorKind::Revoked,
        }
    }

    #[must_use]
    pub const fn unavailable() -> Self {
        Self {
            kind: WebhookCredentialErrorKind::Unavailable,
        }
    }

    #[must_use]
    pub const fn kind(self) -> WebhookCredentialErrorKind {
        self.kind
    }
}

impl fmt::Display for WebhookCredentialError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("webhook credential operation failed")
    }
}

impl std::error::Error for WebhookCredentialError {}

/// Credential boundary used by the default HMAC/mTLS signature authority.
pub trait WebhookCredentialPort {
    /// Resolves the exact current HMAC secret for a credential reference.
    ///
    /// # Errors
    ///
    /// Returns a secret-safe rejection, revocation, or availability failure.
    fn resolve_hmac_secret(
        &mut self,
        reference: &CredentialReferenceId,
    ) -> Result<WebhookHmacSecret, WebhookCredentialError>;

    /// Authorizes one SHA-256 peer-certificate fingerprint for mutual TLS.
    ///
    /// # Errors
    ///
    /// Returns a secret-safe rejection, revocation, or availability failure.
    fn authorize_mtls_peer(
        &mut self,
        reference: &CredentialReferenceId,
        peer_certificate_sha256: &[u8; 32],
    ) -> Result<(), WebhookCredentialError>;
}

/// Credential-isolated HMAC and mTLS operation seam.
pub trait WebhookSignaturePort {
    /// Verifies an HMAC-SHA256 over the timestamp-bound exact raw body.
    ///
    /// # Errors
    ///
    /// Returns a secret-safe credential error.
    fn verify_hmac_sha256(
        &mut self,
        reference: &CredentialReferenceId,
        signed_at_millis: u64,
        payload: &[u8],
        presented_signature: &[u8],
    ) -> Result<(), WebhookCredentialError>;

    /// Produces an HMAC-SHA256 over the timestamp-bound exact outbound body.
    ///
    /// # Errors
    ///
    /// Returns a secret-safe credential error.
    fn sign_hmac_sha256(
        &mut self,
        reference: &CredentialReferenceId,
        signed_at_millis: u64,
        payload: &[u8],
    ) -> Result<[u8; 32], WebhookCredentialError>;

    /// Verifies one peer certificate fingerprint for mutual TLS.
    ///
    /// # Errors
    ///
    /// Returns a secret-safe credential error.
    fn verify_mtls_peer(
        &mut self,
        reference: &CredentialReferenceId,
        peer_certificate_sha256: &[u8; 32],
    ) -> Result<(), WebhookCredentialError>;
}

/// Default signature authority that keeps HMAC key access behind one port.
pub struct CredentialWebhookSignaturePort<Credentials> {
    credentials: Credentials,
}

impl<Credentials> CredentialWebhookSignaturePort<Credentials> {
    #[must_use]
    pub const fn new(credentials: Credentials) -> Self {
        Self { credentials }
    }
}

impl<Credentials> WebhookSignaturePort for CredentialWebhookSignaturePort<Credentials>
where
    Credentials: WebhookCredentialPort,
{
    fn verify_hmac_sha256(
        &mut self,
        reference: &CredentialReferenceId,
        signed_at_millis: u64,
        payload: &[u8],
        presented_signature: &[u8],
    ) -> Result<(), WebhookCredentialError> {
        let secret = self.credentials.resolve_hmac_secret(reference)?;
        let mut hmac = Hmac::<Sha256>::new_from_slice(&secret.0)
            .map_err(|_| WebhookCredentialError::rejected())?;
        update_hmac(&mut hmac, signed_at_millis, payload);
        hmac.verify_slice(presented_signature)
            .map_err(|_| WebhookCredentialError::rejected())
    }

    fn sign_hmac_sha256(
        &mut self,
        reference: &CredentialReferenceId,
        signed_at_millis: u64,
        payload: &[u8],
    ) -> Result<[u8; 32], WebhookCredentialError> {
        let secret = self.credentials.resolve_hmac_secret(reference)?;
        let mut hmac = Hmac::<Sha256>::new_from_slice(&secret.0)
            .map_err(|_| WebhookCredentialError::rejected())?;
        update_hmac(&mut hmac, signed_at_millis, payload);
        Ok(hmac.finalize().into_bytes().into())
    }

    fn verify_mtls_peer(
        &mut self,
        reference: &CredentialReferenceId,
        peer_certificate_sha256: &[u8; 32],
    ) -> Result<(), WebhookCredentialError> {
        self.credentials
            .authorize_mtls_peer(reference, peer_certificate_sha256)
    }
}

/// Transport-authenticated proof kept only in the raw inbound request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WebhookInboundProof {
    HmacSha256 {
        signed_at_millis: u64,
        signature: [u8; 32],
    },
    MutualTls {
        signed_at_millis: u64,
        peer_certificate_sha256: [u8; 32],
    },
}

impl WebhookInboundProof {
    /// Builds an exact HMAC proof.
    ///
    /// # Errors
    ///
    /// Rejects an unsafe timestamp.
    pub fn hmac_sha256(
        signed_at_millis: u64,
        signature: [u8; 32],
    ) -> Result<Self, IntegrationError> {
        validate_timestamp(signed_at_millis)?;
        Ok(Self::HmacSha256 {
            signed_at_millis,
            signature,
        })
    }

    /// Builds an exact mutual-TLS peer proof.
    ///
    /// # Errors
    ///
    /// Rejects an unsafe timestamp.
    pub fn mutual_tls(
        signed_at_millis: u64,
        peer_certificate_sha256: [u8; 32],
    ) -> Result<Self, IntegrationError> {
        validate_timestamp(signed_at_millis)?;
        Ok(Self::MutualTls {
            signed_at_millis,
            peer_certificate_sha256,
        })
    }

    const fn authentication(&self) -> WebhookAuthenticationMode {
        match self {
            Self::HmacSha256 { .. } => WebhookAuthenticationMode::HmacSha256,
            Self::MutualTls { .. } => WebhookAuthenticationMode::MutualTls,
        }
    }

    fn encode(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(41);
        match self {
            Self::HmacSha256 {
                signed_at_millis,
                signature,
            } => {
                bytes.push(1);
                bytes.extend_from_slice(&signed_at_millis.to_be_bytes());
                bytes.extend_from_slice(signature);
            }
            Self::MutualTls {
                signed_at_millis,
                peer_certificate_sha256,
            } => {
                bytes.push(2);
                bytes.extend_from_slice(&signed_at_millis.to_be_bytes());
                bytes.extend_from_slice(peer_certificate_sha256);
            }
        }
        bytes
    }

    fn decode(value: &[u8]) -> Option<Self> {
        if value.len() != 41 {
            return None;
        }
        let signed_at_millis = u64::from_be_bytes(value.get(1..9)?.try_into().ok()?);
        if validate_timestamp(signed_at_millis).is_err() {
            return None;
        }
        let proof: [u8; 32] = value.get(9..41)?.try_into().ok()?;
        match value[0] {
            1 => Some(Self::HmacSha256 {
                signed_at_millis,
                signature: proof,
            }),
            2 => Some(Self::MutualTls {
                signed_at_millis,
                peer_certificate_sha256: proof,
            }),
            _ => None,
        }
    }

    const fn signed_at_millis(&self) -> u64 {
        match self {
            Self::HmacSha256 {
                signed_at_millis, ..
            }
            | Self::MutualTls {
                signed_at_millis, ..
            } => *signed_at_millis,
        }
    }
}

/// Builds bounded generic inbound requests without retaining authentication proof.
#[derive(Clone, Debug)]
pub struct WebhookRequestFactory {
    integration_id: EnterpriseIntegrationId,
    authentication: WebhookAuthenticationMode,
    maximum_body_bytes: usize,
}

impl WebhookRequestFactory {
    #[must_use]
    pub fn new(config: &WebhookConnectorConfig) -> Self {
        Self {
            integration_id: config.integration_id.clone(),
            authentication: config.inbound_policy.authentication,
            maximum_body_bytes: config.limits.inbound_body_bytes,
        }
    }

    /// Builds a framework request from transport-authenticated facts.
    ///
    /// # Errors
    ///
    /// Rejects the wrong authentication mode or an oversized/invalid request.
    pub fn build(
        &self,
        scope: AuditScope,
        metadata: &InboundWebhookMetadata,
        proof: &WebhookInboundProof,
        payload: Vec<u8>,
    ) -> Result<InboundWebhookRequest, IntegrationError> {
        if proof.authentication() != self.authentication
            || payload.is_empty()
            || payload.len() > self.maximum_body_bytes
        {
            return Err(invalid());
        }
        let proof = proof.encode();
        let metadata = InboundWebhookMetadata::try_new(
            metadata.event_type(),
            authenticated_event_identity(&proof, &payload),
            metadata.ordering_key(),
            metadata.provider_sequence(),
            metadata.received_at_millis(),
        )?;
        InboundWebhookRequest::try_new(self.integration_id.clone(), scope, metadata, proof, payload)
    }
}

/// Trusted request-time source used for signature windows and outbound signing.
pub trait WebhookClock {
    #[must_use]
    fn now_millis(&self) -> u64;
}

/// Existing-framework verifier for HMAC and mTLS generic webhook proofs.
pub struct GenericWebhookVerifier<Signature, Clock> {
    integration_id: EnterpriseIntegrationId,
    policy: WebhookInboundPolicy,
    signature: Signature,
    clock: Clock,
}

impl<Signature, Clock> GenericWebhookVerifier<Signature, Clock> {
    #[must_use]
    pub fn new(config: &WebhookConnectorConfig, signature: Signature, clock: Clock) -> Self {
        Self {
            integration_id: config.integration_id.clone(),
            policy: config.inbound_policy,
            signature,
            clock,
        }
    }
}

impl<Signature, Clock> WebhookSignatureVerifier for GenericWebhookVerifier<Signature, Clock>
where
    Signature: WebhookSignaturePort,
    Clock: WebhookClock,
{
    fn verify(
        &mut self,
        authority: &ConnectorAuthority,
        signature: &[u8],
        payload: &[u8],
    ) -> Result<(), SignatureVerificationError> {
        require_authority(authority, &self.integration_id)
            .map_err(|_| SignatureVerificationError::rejected())?;
        let proof = WebhookInboundProof::decode(signature)
            .filter(|proof| proof.authentication() == self.policy.authentication)
            .ok_or_else(SignatureVerificationError::rejected)?;
        if !timestamp_in_window(
            self.clock.now_millis(),
            proof.signed_at_millis(),
            self.policy,
        ) {
            return Err(SignatureVerificationError::rejected());
        }
        let result = match proof {
            WebhookInboundProof::HmacSha256 {
                signed_at_millis,
                signature,
            } => self.signature.verify_hmac_sha256(
                authority.credential_reference_id(),
                signed_at_millis,
                payload,
                &signature,
            ),
            WebhookInboundProof::MutualTls {
                peer_certificate_sha256,
                ..
            } => self.signature.verify_mtls_peer(
                authority.credential_reference_id(),
                &peer_certificate_sha256,
            ),
        };
        result.map_err(signature_error)
    }
}

/// Provider-neutral webhook configuration shared by inbound and outbound paths.
#[derive(Clone, Debug)]
pub struct WebhookConnectorConfig {
    integration_id: EnterpriseIntegrationId,
    endpoint: WebhookEndpoint,
    inbound_policy: WebhookInboundPolicy,
    mappings: BTreeMap<String, WebhookMappingTemplate>,
    limits: WebhookLimits,
}

impl WebhookConnectorConfig {
    /// Builds a closed generic webhook configuration.
    ///
    /// # Errors
    ///
    /// Rejects invalid identity, missing/duplicate mappings, or unsafe limits.
    pub fn try_new(
        integration_id: EnterpriseIntegrationId,
        endpoint: WebhookEndpoint,
        inbound_policy: WebhookInboundPolicy,
        templates: Vec<WebhookMappingTemplate>,
        limits: WebhookLimits,
    ) -> Result<Self, IntegrationError> {
        validate_integration_id(&integration_id)?;
        if templates.is_empty() || templates.len() > MAX_MAPPING_TEMPLATES {
            return Err(invalid());
        }
        let mut mappings = BTreeMap::new();
        for template in templates {
            let key = template.event_type.clone();
            if mappings.insert(key, template).is_some() {
                return Err(invalid());
            }
        }
        Ok(Self {
            integration_id,
            endpoint,
            inbound_policy,
            mappings,
            limits,
        })
    }

    #[must_use]
    pub const fn integration_id(&self) -> &EnterpriseIntegrationId {
        &self.integration_id
    }

    #[must_use]
    pub const fn endpoint(&self) -> &WebhookEndpoint {
        &self.endpoint
    }

    #[must_use]
    pub const fn inbound_policy(&self) -> WebhookInboundPolicy {
        self.inbound_policy
    }

    #[must_use]
    pub const fn limits(&self) -> WebhookLimits {
        self.limits
    }
}

/// DNS seam whose result is pinned into the subsequent HTTPS request.
pub trait WebhookAddressResolverPort {
    /// Resolves the configured host exactly once for one delivery attempt.
    ///
    /// # Errors
    ///
    /// Returns a stable connector error on lookup failure.
    fn resolve(&mut self, host: &str, port: u16) -> Result<Vec<IpAddr>, ConnectorCallError>;
}

/// Authentication material prepared for one exact outbound attempt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WebhookOutboundAuthentication {
    HmacSha256 {
        signed_at_millis: u64,
        signature: [u8; 32],
    },
    MutualTls {
        credential_reference_id: CredentialReferenceId,
    },
}

/// Fully validated HTTPS call. Implementations must connect only to one of the
/// pinned addresses while retaining the endpoint host for TLS SNI validation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WebhookHttpRequest {
    endpoint: WebhookEndpoint,
    addresses: Vec<SocketAddr>,
    operation_key: crate::IntegrationOperationKey,
    body: Vec<u8>,
    authentication: WebhookOutboundAuthentication,
    timeout_millis: u64,
}

impl WebhookHttpRequest {
    #[must_use]
    pub const fn endpoint(&self) -> &WebhookEndpoint {
        &self.endpoint
    }

    #[must_use]
    pub fn addresses(&self) -> &[SocketAddr] {
        &self.addresses
    }

    #[must_use]
    pub const fn operation_key(&self) -> &crate::IntegrationOperationKey {
        &self.operation_key
    }

    #[must_use]
    pub fn body(&self) -> &[u8] {
        &self.body
    }

    #[must_use]
    pub const fn authentication(&self) -> &WebhookOutboundAuthentication {
        &self.authentication
    }

    #[must_use]
    pub const fn timeout_millis(&self) -> u64 {
        self.timeout_millis
    }
}

/// Bounded provider response returned by the HTTPS port.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WebhookHttpResponse {
    status: u16,
    body: Vec<u8>,
    retry_after_millis: Option<u64>,
}

impl WebhookHttpResponse {
    /// Builds a provider response for generic status handling.
    ///
    /// # Errors
    ///
    /// Rejects invalid status, absolute response size, or retry delay.
    pub fn try_new(
        status: u16,
        body: Vec<u8>,
        retry_after_millis: Option<u64>,
    ) -> Result<Self, IntegrationError> {
        if !(100..=599).contains(&status)
            || body.len() > ABSOLUTE_MAX_RESPONSE_BYTES
            || retry_after_millis.is_some_and(|value| value == 0 || value > MAX_SAFE_INTEGER)
        {
            return Err(invalid());
        }
        Ok(Self {
            status,
            body,
            retry_after_millis,
        })
    }

    #[must_use]
    pub const fn status(&self) -> u16 {
        self.status
    }
}

/// HTTPS port consuming only an allowlisted host and pinned public addresses.
pub trait WebhookHttpPort {
    /// Performs one outbound call without following redirects.
    ///
    /// # Errors
    ///
    /// Returns a stable, secret-free connector error.
    fn send(
        &mut self,
        request: &WebhookHttpRequest,
    ) -> Result<WebhookHttpResponse, ConnectorCallError>;
}

/// Provider-neutral webhook protocol mapper and outbound adapter.
pub struct GenericWebhookConnector<Resolver, Transport, Signature, Clock> {
    config: WebhookConnectorConfig,
    resolver: Resolver,
    transport: Transport,
    signature: Signature,
    clock: Clock,
}

impl<Resolver, Transport, Signature, Clock>
    GenericWebhookConnector<Resolver, Transport, Signature, Clock>
{
    #[must_use]
    pub const fn new(
        config: WebhookConnectorConfig,
        resolver: Resolver,
        transport: Transport,
        signature: Signature,
        clock: Clock,
    ) -> Self {
        Self {
            config,
            resolver,
            transport,
            signature,
            clock,
        }
    }
}

impl<Resolver, Transport, Signature, Clock> ConnectorPort
    for GenericWebhookConnector<Resolver, Transport, Signature, Clock>
where
    Resolver: WebhookAddressResolverPort,
    Transport: WebhookHttpPort,
    Signature: WebhookSignaturePort,
    Clock: WebhookClock,
{
    fn normalize_inbound(
        &mut self,
        authority: &ConnectorAuthority,
        context: &InboundNormalizationContext,
        payload: &[u8],
    ) -> Result<NormalizedInboundEvent, ConnectorCallError> {
        require_authority(authority, &self.config.integration_id)
            .map_err(|_| permanent_error("WEBHOOK_AUTHORITY_MISMATCH"))?;
        if payload.len() > self.config.limits.inbound_body_bytes {
            return Err(permanent_error("WEBHOOK_PAYLOAD_TOO_LARGE"));
        }
        self.config
            .mappings
            .get(context.event_type())
            .ok_or_else(|| permanent_error("WEBHOOK_EVENT_UNSUPPORTED"))?
            .map(payload)
    }

    fn deliver_outbound(
        &mut self,
        claim: &OutboundClaim,
    ) -> Result<OutboundCallReceipt, ConnectorCallError> {
        require_authority(claim.authority(), &self.config.integration_id)
            .map_err(|_| permanent_error("WEBHOOK_AUTHORITY_MISMATCH"))?;
        let body = canonical_outbound_body(claim)?;
        if body.len() > self.config.limits.outbound_body_bytes {
            return Err(permanent_error("WEBHOOK_PAYLOAD_TOO_LARGE"));
        }
        let addresses = self
            .resolver
            .resolve(self.config.endpoint.host(), self.config.endpoint.port())?;
        let addresses = pinned_public_addresses(addresses, self.config.endpoint.port())?;
        let now_millis = self.clock.now_millis();
        if validate_timestamp(now_millis).is_err() {
            return Err(retryable_error("WEBHOOK_CLOCK_INVALID"));
        }
        let authentication = match self.config.inbound_policy.authentication {
            WebhookAuthenticationMode::HmacSha256 => {
                let signature = self
                    .signature
                    .sign_hmac_sha256(
                        claim.authority().credential_reference_id(),
                        now_millis,
                        &body,
                    )
                    .map_err(outbound_credential_error)?;
                WebhookOutboundAuthentication::HmacSha256 {
                    signed_at_millis: now_millis,
                    signature,
                }
            }
            WebhookAuthenticationMode::MutualTls => WebhookOutboundAuthentication::MutualTls {
                credential_reference_id: claim.authority().credential_reference_id().clone(),
            },
        };
        let request = WebhookHttpRequest {
            endpoint: self.config.endpoint.clone(),
            addresses,
            operation_key: claim.operation_key().clone(),
            body,
            authentication,
            timeout_millis: self.config.limits.timeout_millis,
        };
        let response = self.transport.send(&request)?;
        classify_response(&response, self.config.limits.response_body_bytes)
    }
}

fn canonical_outbound_body(claim: &OutboundClaim) -> Result<Vec<u8>, ConnectorCallError> {
    let payload: Value = serde_json::from_slice(claim.payload())
        .map_err(|_| permanent_error("WEBHOOK_PAYLOAD_INVALID"))?;
    let mut envelope = BTreeMap::new();
    envelope.insert(
        "eventType".to_owned(),
        Value::String(claim.operation_name().to_owned()),
    );
    envelope.insert(
        "idempotencyKey".to_owned(),
        Value::String(claim.operation_key().digest().0.clone()),
    );
    envelope.insert("payload".to_owned(), payload);
    envelope.insert(
        "schema".to_owned(),
        Value::String("winwincode.webhook.delivery.v1".to_owned()),
    );
    serde_json::to_vec(&envelope).map_err(|_| permanent_error("WEBHOOK_PAYLOAD_INVALID"))
}

fn update_hmac(hmac: &mut Hmac<Sha256>, signed_at_millis: u64, payload: &[u8]) {
    hmac.update(&signed_at_millis.to_be_bytes());
    hmac.update(b".");
    hmac.update(payload);
}

fn authenticated_event_identity(proof: &[u8], payload: &[u8]) -> String {
    let mut hash = Sha256::new();
    hash.update(b"winwincode.integration.authenticated-webhook-event.v1");
    hash.update([0]);
    hash.update(proof);
    hash.update(payload);
    format!("authenticated:{:x}", hash.finalize())
}

fn classify_response(
    response: &WebhookHttpResponse,
    maximum_body_bytes: usize,
) -> Result<OutboundCallReceipt, ConnectorCallError> {
    if response.body.len() > maximum_body_bytes {
        return Err(permanent_error("WEBHOOK_RESPONSE_TOO_LARGE"));
    }
    if (200..=299).contains(&response.status) {
        let mut hash = Sha256::new();
        hash.update(b"winwincode.integration.webhook-response.v1");
        hash.update([0]);
        hash.update(response.status.to_be_bytes());
        hash.update(&response.body);
        return OutboundCallReceipt::try_new(
            Sha256Digest(format!("sha256:{:x}", hash.finalize())),
            true,
        )
        .map_err(|_| permanent_error("WEBHOOK_RESPONSE_INVALID"));
    }
    if matches!(response.status, 408 | 425 | 429 | 500..=599) {
        if let Some(delay) = response.retry_after_millis {
            let error = ConnectorCallError::retryable_after("WEBHOOK_REMOTE_RETRYABLE", delay)
                .map_err(|_| permanent_error("WEBHOOK_RESPONSE_INVALID"))?;
            return Err(error);
        }
        return Err(retryable_error("WEBHOOK_REMOTE_RETRYABLE"));
    }
    Err(permanent_error("WEBHOOK_REMOTE_REJECTED"))
}

fn pinned_public_addresses(
    addresses: Vec<IpAddr>,
    port: u16,
) -> Result<Vec<SocketAddr>, ConnectorCallError> {
    let addresses = addresses.into_iter().collect::<BTreeSet<_>>();
    if addresses.is_empty()
        || addresses.len() > MAX_RESOLVED_ADDRESSES
        || addresses.iter().any(|address| !public_address(*address))
    {
        return Err(permanent_error("WEBHOOK_ENDPOINT_BLOCKED"));
    }
    Ok(addresses
        .into_iter()
        .map(|address| SocketAddr::new(address, port))
        .collect())
}

fn public_address(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => public_ipv4(address),
        IpAddr::V6(address) => public_ipv6(address),
    }
}

fn public_ipv4(address: Ipv4Addr) -> bool {
    let octets = address.octets();
    !address.is_private()
        && !address.is_loopback()
        && !address.is_link_local()
        && !address.is_broadcast()
        && !address.is_documentation()
        && !address.is_unspecified()
        && !address.is_multicast()
        && octets[0] != 0
        && !(octets[0] == 100 && (64..=127).contains(&octets[1]))
        && !(octets[0] == 192 && octets[1] == 0 && octets[2] == 0)
        && !(octets[0] == 192 && octets[1] == 88 && octets[2] == 99)
        && !(octets[0] == 198 && matches!(octets[1], 18 | 19))
        && octets[0] < 240
}

fn public_ipv6(address: Ipv6Addr) -> bool {
    if let Some(mapped) = address.to_ipv4_mapped() {
        return public_ipv4(mapped);
    }
    let first = address.segments()[0];
    !address.is_unspecified()
        && !address.is_loopback()
        && !address.is_multicast()
        && first & 0xfe00 != 0xfc00
        && first & 0xffc0 != 0xfe80
        && first & 0xffc0 != 0xfec0
        && address.segments()[0..2] != [0x2001, 0x0db8]
        && first & 0xe000 == 0x2000
}

fn valid_hostname(value: &str) -> bool {
    if value.is_empty()
        || value.len() > 253
        || value.ends_with('.')
        || value == "localhost"
        || value.ends_with(".localhost")
        || value.contains('*')
    {
        return false;
    }
    if value.parse::<IpAddr>().is_ok() {
        return true;
    }
    value.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    })
}

fn valid_output_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_alphabetic())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn valid_json_pointer(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 512
        && value.starts_with('/')
        && value.split('/').skip(1).all(|segment| {
            !segment.is_empty()
                && !segment
                    .as_bytes()
                    .windows(2)
                    .any(|pair| pair[0] == b'~' && !matches!(pair.get(1), Some(b'0' | b'1')))
                && !segment.ends_with('~')
        })
}

fn valid_portable_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'-' | b'_')
        })
}

fn validate_timestamp(value: u64) -> Result<(), IntegrationError> {
    if value == 0 || value > MAX_SAFE_INTEGER {
        Err(invalid())
    } else {
        Ok(())
    }
}

fn timestamp_in_window(now: u64, signed_at: u64, policy: WebhookInboundPolicy) -> bool {
    if validate_timestamp(now).is_err() || validate_timestamp(signed_at).is_err() {
        return false;
    }
    signed_at.checked_sub(now).map_or_else(
        || now.saturating_sub(signed_at) <= policy.maximum_age_millis,
        |future| future <= policy.maximum_future_skew_millis,
    )
}

fn require_authority(
    authority: &ConnectorAuthority,
    integration_id: &EnterpriseIntegrationId,
) -> Result<(), IntegrationError> {
    if authority.integration_id() == integration_id
        && authority.protocol().as_str() == WEBHOOK_CONNECTOR_PROTOCOL
    {
        Ok(())
    } else {
        Err(IntegrationError::new(
            IntegrationErrorKind::TenantMismatch,
            "webhook authority does not match",
        ))
    }
}

const fn signature_error(error: WebhookCredentialError) -> SignatureVerificationError {
    match error.kind() {
        WebhookCredentialErrorKind::Revoked => SignatureVerificationError::credential_revoked(),
        WebhookCredentialErrorKind::Rejected | WebhookCredentialErrorKind::Unavailable => {
            SignatureVerificationError::rejected()
        }
    }
}

fn outbound_credential_error(error: WebhookCredentialError) -> ConnectorCallError {
    match error.kind() {
        WebhookCredentialErrorKind::Revoked => connector_error(
            ConnectorCallErrorKind::CredentialRevoked,
            "WEBHOOK_CREDENTIAL_REVOKED",
        ),
        WebhookCredentialErrorKind::Unavailable => {
            retryable_error("WEBHOOK_CREDENTIAL_UNAVAILABLE")
        }
        WebhookCredentialErrorKind::Rejected => permanent_error("WEBHOOK_CREDENTIAL_REJECTED"),
    }
}

fn retryable_error(code: &str) -> ConnectorCallError {
    connector_error(ConnectorCallErrorKind::Retryable, code)
}

fn permanent_error(code: &str) -> ConnectorCallError {
    connector_error(ConnectorCallErrorKind::Permanent, code)
}

fn connector_error(kind: ConnectorCallErrorKind, code: &str) -> ConnectorCallError {
    ConnectorCallError::try_new(kind, code).expect("static webhook error code must be valid")
}

const fn invalid() -> IntegrationError {
    IntegrationError::new(
        IntegrationErrorKind::Invalid,
        "webhook configuration is invalid",
    )
}
