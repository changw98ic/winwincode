// SPDX-License-Identifier: Apache-2.0

//! Verified HTTPS transport for the enterprise Vault/KMS `SecretStore`.
//!
//! The adapter resolves a short-lived workload credential for each request,
//! sends one canonical scope-bound operation over pinned TLS, retries with the
//! same operation identity, and accepts only bounded canonical responses. It
//! never stores a Vault token, Credential secret, or remote response text.

use std::{fmt, io::Read as _, str, sync::Arc, time::Duration};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::{
    Deserialize, Deserializer, Serialize, Serializer,
    de::{self, Visitor},
};
use sha2::{Digest, Sha256};

use crate::{
    CredentialReferenceResolution, ResolvedSecret, SecretStoreError, SecretStorePort, VaultKmsClock,
};

const REQUEST_SCHEMA: &str = "winwincode.vault-kms-network-request.v1";
const RESPONSE_SCHEMA: &str = "winwincode.vault-kms-network-response.v1";
const MAX_ENDPOINT_BYTES: usize = 2_048;
const MAX_TOKEN_BYTES: usize = 16 * 1024;
const MAX_SECRET_BYTES: usize = 1024 * 1024;
const MAX_RESPONSE_BYTES: usize = 2 * 1024 * 1024;
const MAX_OPERATION_ATTEMPTS: u8 = 3;
const MAX_WORKLOAD_CREDENTIAL_TTL_MS: u64 = 15 * 60 * 1000;

/// TLS trust roots for one Vault/KMS endpoint.
#[derive(Clone)]
pub enum VaultKmsNetworkTlsRoots {
    /// Mozilla `WebPKI` roots shipped by the pinned HTTP stack.
    WebPki,
    /// Explicit DER roots for a private deployment or reproducible TLS gate.
    Specific(Vec<Vec<u8>>),
}

impl fmt::Debug for VaultKmsNetworkTlsRoots {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WebPki => formatter.write_str("VaultKmsNetworkTlsRoots::WebPki"),
            Self::Specific(roots) => formatter
                .debug_tuple("VaultKmsNetworkTlsRoots::Specific")
                .field(&roots.len())
                .finish(),
        }
    }
}

/// Network deadlines for one Vault/KMS operation.
#[derive(Clone, Copy, Debug)]
pub struct VaultKmsNetworkTimeouts {
    pub connect: Duration,
    pub response: Duration,
    pub total: Duration,
}

/// Bounded verified HTTPS configuration for one Vault/KMS service.
#[derive(Clone)]
pub struct VaultKmsNetworkConfig {
    endpoint: String,
    timeouts: VaultKmsNetworkTimeouts,
    max_response_bytes: usize,
    max_attempts: u8,
    tls_roots: VaultKmsNetworkTlsRoots,
}

impl VaultKmsNetworkConfig {
    /// Creates a `WebPKI`-verified Vault/KMS configuration.
    ///
    /// # Errors
    ///
    /// Rejects non-HTTPS or credential-bearing endpoints, unsafe deadlines,
    /// response sizes, and retry counts.
    pub fn try_new(
        endpoint: String,
        timeouts: VaultKmsNetworkTimeouts,
        max_response_bytes: usize,
        max_attempts: u8,
    ) -> Result<Self, SecretStoreError> {
        let config = Self {
            endpoint,
            timeouts,
            max_response_bytes,
            max_attempts,
            tls_roots: VaultKmsNetworkTlsRoots::WebPki,
        };
        config.validate()?;
        Ok(config)
    }

    /// Installs an explicit non-empty DER trust set.
    ///
    /// # Errors
    ///
    /// Rejects empty or oversized trust sets and certificates.
    pub fn with_specific_tls_roots(
        mut self,
        roots: Vec<Vec<u8>>,
    ) -> Result<Self, SecretStoreError> {
        if roots.is_empty()
            || roots.len() > 32
            || roots
                .iter()
                .any(|root| root.is_empty() || root.len() > 64 * 1024)
        {
            return Err(SecretStoreError::corrupt());
        }
        self.tls_roots = VaultKmsNetworkTlsRoots::Specific(roots);
        self.validate()?;
        Ok(self)
    }

    fn validate(&self) -> Result<(), SecretStoreError> {
        if !canonical_https_endpoint(&self.endpoint)
            || self.timeouts.connect.is_zero()
            || self.timeouts.response.is_zero()
            || self.timeouts.total.is_zero()
            || self.timeouts.connect > self.timeouts.total
            || self.timeouts.response > self.timeouts.total
            || self.max_response_bytes == 0
            || self.max_response_bytes > MAX_RESPONSE_BYTES
            || self.max_attempts == 0
            || self.max_attempts > MAX_OPERATION_ATTEMPTS
        {
            return Err(SecretStoreError::corrupt());
        }
        Ok(())
    }
}

impl fmt::Debug for VaultKmsNetworkConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("VaultKmsNetworkConfig")
            .field("endpoint", &"[REDACTED]")
            .field("timeouts", &self.timeouts)
            .field("max_response_bytes", &self.max_response_bytes)
            .field("max_attempts", &self.max_attempts)
            .field("tls_roots", &self.tls_roots)
            .finish()
    }
}

/// One short-lived workload credential acquired immediately before TLS.
pub struct VaultKmsWorkloadCredential {
    token: ResolvedSecret,
    expires_at_ms: u64,
}

impl VaultKmsWorkloadCredential {
    /// Owns a workload token and its authoritative expiry.
    ///
    /// # Errors
    ///
    /// Rejects empty tokens and an absent expiry.
    pub fn try_new(token: Vec<u8>, expires_at_ms: u64) -> Result<Self, SecretStoreError> {
        if expires_at_ms == 0 {
            return Err(SecretStoreError::corrupt());
        }
        Ok(Self {
            token: ResolvedSecret::from_bytes(token)?,
            expires_at_ms,
        })
    }

    #[must_use]
    pub const fn expires_at_ms(&self) -> u64 {
        self.expires_at_ms
    }
}

impl fmt::Debug for VaultKmsWorkloadCredential {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("VaultKmsWorkloadCredential")
            .field("token", &"[REDACTED]")
            .field("expires_at_ms", &self.expires_at_ms)
            .finish()
    }
}

/// Deployment identity source used once per Vault/KMS request.
pub trait VaultKmsWorkloadIdentityPort: Send + Sync {
    /// Issues one bounded workload credential.
    ///
    /// # Errors
    ///
    /// Fails closed when workload identity is unavailable.
    fn issue(&self) -> Result<VaultKmsWorkloadCredential, SecretStoreError>;
}

/// Secret-free read lease returned by the remote Vault/KMS authority.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VaultKmsNetworkLeaseReceipt {
    lease_id: String,
    rotation_version: u64,
    issued_at_ms: u64,
    expires_at_ms: u64,
}

impl VaultKmsNetworkLeaseReceipt {
    #[must_use]
    pub fn lease_id(&self) -> &str {
        &self.lease_id
    }

    #[must_use]
    pub const fn rotation_version(&self) -> u64 {
        self.rotation_version
    }

    #[must_use]
    pub const fn issued_at_ms(&self) -> u64 {
        self.issued_at_ms
    }

    #[must_use]
    pub const fn expires_at_ms(&self) -> u64 {
        self.expires_at_ms
    }
}

/// Secret plus its remote short-lived read lease.
pub struct VaultKmsNetworkLeasedSecret {
    secret: ResolvedSecret,
    receipt: VaultKmsNetworkLeaseReceipt,
}

impl VaultKmsNetworkLeasedSecret {
    #[must_use]
    pub const fn receipt(&self) -> &VaultKmsNetworkLeaseReceipt {
        &self.receipt
    }

    #[must_use]
    pub fn into_secret(self) -> ResolvedSecret {
        self.secret
    }
}

impl fmt::Debug for VaultKmsNetworkLeasedSecret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("VaultKmsNetworkLeasedSecret")
            .field("secret", &"[REDACTED]")
            .field("receipt", &self.receipt)
            .finish()
    }
}

/// Secret-free receipt for a remote immutable Credential version.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VaultKmsNetworkWriteReceipt {
    rotation_version: u64,
    key_version: u64,
    replayed: bool,
}

impl VaultKmsNetworkWriteReceipt {
    #[must_use]
    pub const fn rotation_version(&self) -> u64 {
        self.rotation_version
    }

    #[must_use]
    pub const fn key_version(&self) -> u64 {
        self.key_version
    }

    #[must_use]
    pub const fn replayed(&self) -> bool {
        self.replayed
    }
}

/// Secret-free remote revocation receipt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VaultKmsNetworkRevocationReceipt {
    removed_versions: u64,
}

/// Secret-free receipt for a remote customer-key rotation and rewrap.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VaultKmsNetworkKeyRotationReceipt {
    key_version: u64,
    rewrapped_versions: u64,
}

impl VaultKmsNetworkKeyRotationReceipt {
    #[must_use]
    pub const fn key_version(self) -> u64 {
        self.key_version
    }

    #[must_use]
    pub const fn rewrapped_versions(self) -> u64 {
        self.rewrapped_versions
    }
}

impl VaultKmsNetworkRevocationReceipt {
    #[must_use]
    pub const fn removed_versions(self) -> u64 {
        self.removed_versions
    }
}

/// Verified HTTPS adapter for a workload-authenticated Vault/KMS service.
pub struct VaultKmsNetworkAdapter {
    config: VaultKmsNetworkConfig,
    agent: ureq::Agent,
    identity: Arc<dyn VaultKmsWorkloadIdentityPort>,
    clock: Arc<dyn VaultKmsClock>,
}

impl VaultKmsNetworkAdapter {
    /// Builds a no-proxy, pinned-rustls Vault/KMS client.
    ///
    /// # Errors
    ///
    /// Rejects malformed roots and invalid network configuration.
    pub fn try_new(
        config: VaultKmsNetworkConfig,
        identity: Arc<dyn VaultKmsWorkloadIdentityPort>,
        clock: Arc<dyn VaultKmsClock>,
    ) -> Result<Self, SecretStoreError> {
        config.validate()?;
        let root_certs = match &config.tls_roots {
            VaultKmsNetworkTlsRoots::WebPki => ureq::tls::RootCerts::WebPki,
            VaultKmsNetworkTlsRoots::Specific(values) => values
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
            config,
            agent,
            identity,
            clock,
        })
    }

    /// Publishes one exact immutable secret version remotely.
    ///
    /// # Errors
    ///
    /// Exact operation retries replay; changed bytes conflict remotely.
    pub fn store(
        &self,
        reference: &CredentialReferenceResolution,
        secret: ResolvedSecret,
    ) -> Result<VaultKmsNetworkWriteReceipt, SecretStoreError> {
        self.write(reference, reference.rotation_version(), secret)
    }

    /// Stages the next remote secret version before metadata advances.
    ///
    /// # Errors
    ///
    /// Rejects the supported version boundary and changed replay bytes.
    pub fn rotate_secret(
        &self,
        current: &CredentialReferenceResolution,
        secret: ResolvedSecret,
    ) -> Result<VaultKmsNetworkWriteReceipt, SecretStoreError> {
        let next_version = current
            .rotation_version()
            .checked_add(1)
            .filter(|version| *version <= 9_007_199_254_740_991)
            .ok_or_else(SecretStoreError::version_conflict)?;
        self.write(current, next_version, secret)
    }

    /// Resolves one exact remote version with its bounded read lease.
    ///
    /// # Errors
    ///
    /// Missing, revoked, expired, malformed, or unavailable remote state fails
    /// with the canonical secret-safe error categories.
    pub fn resolve_lease(
        &self,
        reference: &CredentialReferenceResolution,
    ) -> Result<VaultKmsNetworkLeasedSecret, SecretStoreError> {
        let operation_id = operation_id("resolve", reference, reference.rotation_version())?;
        let request = ProtocolRequest::Resolve {
            schema: REQUEST_SCHEMA,
            operation_id: operation_id.clone(),
            reference,
        };
        let response = self.execute(request)?;
        let ProtocolResponse::Lease {
            schema,
            operation_id: returned_id,
            rotation_version,
            lease_id,
            issued_at_ms,
            expires_at_ms,
            secret,
        } = response
        else {
            return Err(SecretStoreError::corrupt());
        };
        validate_response_identity(&schema, &operation_id, &returned_id)?;
        validate_lease(
            reference.rotation_version(),
            rotation_version,
            &lease_id,
            issued_at_ms,
            expires_at_ms,
            self.clock
                .now_ms()
                .map_err(|_| SecretStoreError::unavailable())?,
        )?;
        let bytes = STANDARD
            .decode(&secret.0)
            .map_err(|_| SecretStoreError::corrupt())?;
        if bytes.len() > MAX_SECRET_BYTES {
            return Err(SecretStoreError::corrupt());
        }
        Ok(VaultKmsNetworkLeasedSecret {
            secret: ResolvedSecret::from_bytes(bytes)?,
            receipt: VaultKmsNetworkLeaseReceipt {
                lease_id,
                rotation_version,
                issued_at_ms,
                expires_at_ms,
            },
        })
    }

    /// Renews an unexpired read lease without returning secret material.
    ///
    /// # Errors
    ///
    /// Expired, revoked, foreign, or unavailable leases fail closed.
    pub fn renew_lease(
        &self,
        reference: &CredentialReferenceResolution,
        current: &VaultKmsNetworkLeaseReceipt,
    ) -> Result<VaultKmsNetworkLeaseReceipt, SecretStoreError> {
        if current.rotation_version != reference.rotation_version() {
            return Err(SecretStoreError::version_conflict());
        }
        let operation_id = operation_id("renew", reference, reference.rotation_version())?;
        let request = ProtocolRequest::Renew {
            schema: REQUEST_SCHEMA,
            operation_id: operation_id.clone(),
            reference,
            lease_id: &current.lease_id,
        };
        let response = self.execute(request)?;
        let ProtocolResponse::Renewed {
            schema,
            operation_id: returned_id,
            rotation_version,
            lease_id,
            issued_at_ms,
            expires_at_ms,
        } = response
        else {
            return Err(SecretStoreError::corrupt());
        };
        validate_response_identity(&schema, &operation_id, &returned_id)?;
        validate_lease(
            reference.rotation_version(),
            rotation_version,
            &lease_id,
            issued_at_ms,
            expires_at_ms,
            self.clock
                .now_ms()
                .map_err(|_| SecretStoreError::unavailable())?,
        )?;
        Ok(VaultKmsNetworkLeaseReceipt {
            lease_id,
            rotation_version,
            issued_at_ms,
            expires_at_ms,
        })
    }

    /// Revokes the remote reference before encrypted versions are removed.
    ///
    /// # Errors
    ///
    /// Network, authorization, and corrupt response failures remain secret-safe.
    pub fn revoke(
        &self,
        reference: &CredentialReferenceResolution,
    ) -> Result<VaultKmsNetworkRevocationReceipt, SecretStoreError> {
        let operation_id = operation_id("revoke", reference, reference.rotation_version())?;
        let request = ProtocolRequest::Revoke {
            schema: REQUEST_SCHEMA,
            operation_id: operation_id.clone(),
            reference,
        };
        let response = self.execute(request)?;
        let ProtocolResponse::Revoked {
            schema,
            operation_id: returned_id,
            removed_versions,
        } = response
        else {
            return Err(SecretStoreError::corrupt());
        };
        validate_response_identity(&schema, &operation_id, &returned_id)?;
        Ok(VaultKmsNetworkRevocationReceipt { removed_versions })
    }

    /// Activates a new remote customer-key version and rewraps stored values.
    ///
    /// No key bytes cross this API: the remote Vault/KMS authority owns the
    /// requested key version and reports only secret-free counts.
    ///
    /// # Errors
    ///
    /// Rejects invalid versions, non-monotonic remote state, and unavailable
    /// or malformed responses.
    pub fn rotate_customer_key(
        &self,
        key_version: u64,
    ) -> Result<VaultKmsNetworkKeyRotationReceipt, SecretStoreError> {
        if key_version == 0 || key_version > 9_007_199_254_740_991 {
            return Err(SecretStoreError::version_conflict());
        }
        let operation_id = key_operation_id(key_version)?;
        let request = ProtocolRequest::RotateKey {
            schema: REQUEST_SCHEMA,
            operation_id: operation_id.clone(),
            key_version,
        };
        let response = self.execute(request)?;
        let ProtocolResponse::KeyRotated {
            schema,
            operation_id: returned_id,
            key_version: returned_version,
            rewrapped_versions,
        } = response
        else {
            return Err(SecretStoreError::corrupt());
        };
        validate_response_identity(&schema, &operation_id, &returned_id)?;
        if returned_version != key_version {
            return Err(SecretStoreError::corrupt());
        }
        Ok(VaultKmsNetworkKeyRotationReceipt {
            key_version,
            rewrapped_versions,
        })
    }

    fn write(
        &self,
        reference: &CredentialReferenceResolution,
        rotation_version: u64,
        secret: ResolvedSecret,
    ) -> Result<VaultKmsNetworkWriteReceipt, SecretStoreError> {
        if secret.expose().len() > MAX_SECRET_BYTES {
            return Err(SecretStoreError::corrupt());
        }
        let operation_id = operation_id("write", reference, rotation_version)?;
        let encoded_secret = SensitiveString(STANDARD.encode(secret.expose()).into_bytes());
        drop(secret);
        let request = ProtocolRequest::Write {
            schema: REQUEST_SCHEMA,
            operation_id: operation_id.clone(),
            reference,
            rotation_version,
            secret: encoded_secret,
        };
        let response = self.execute(request)?;
        let ProtocolResponse::Written {
            schema,
            operation_id: returned_id,
            rotation_version: returned_version,
            key_version,
            replayed,
        } = response
        else {
            return Err(SecretStoreError::corrupt());
        };
        validate_response_identity(&schema, &operation_id, &returned_id)?;
        if returned_version != rotation_version || key_version == 0 {
            return Err(SecretStoreError::corrupt());
        }
        Ok(VaultKmsNetworkWriteReceipt {
            rotation_version,
            key_version,
            replayed,
        })
    }

    fn execute(&self, request: ProtocolRequest<'_>) -> Result<ProtocolResponse, SecretStoreError> {
        let body =
            SensitiveBytes(serde_json::to_vec(&request).map_err(|_| SecretStoreError::corrupt())?);
        drop(request);
        if body.0.len() > MAX_RESPONSE_BYTES {
            return Err(SecretStoreError::corrupt());
        }
        for attempt in 1..=self.config.max_attempts {
            match self.send_once(&body.0) {
                Ok(response) => return parse_response(&response, self.config.max_response_bytes),
                Err(error)
                    if error.kind() == crate::SecretStoreErrorKind::Unavailable
                        && attempt < self.config.max_attempts => {}
                Err(error) => return Err(error),
            }
        }
        Err(SecretStoreError::unavailable())
    }

    fn send_once(&self, body: &[u8]) -> Result<SensitiveBytes, SecretStoreError> {
        let credential = self.identity.issue()?;
        let now_ms = self
            .clock
            .now_ms()
            .map_err(|_| SecretStoreError::unavailable())?;
        if credential.expires_at_ms <= now_ms
            || credential.expires_at_ms.saturating_sub(now_ms) > MAX_WORKLOAD_CREDENTIAL_TTL_MS
            || credential.token.expose().len() > MAX_TOKEN_BYTES
        {
            return Err(SecretStoreError::unavailable());
        }
        let mut authorization =
            SensitiveBytes(Vec::with_capacity(7 + credential.token.expose().len()));
        authorization.0.extend_from_slice(b"Bearer ");
        authorization.0.extend_from_slice(credential.token.expose());
        let authorization_text = str::from_utf8(&authorization.0)
            .ok()
            .filter(|value| valid_header_value(value))
            .ok_or_else(SecretStoreError::unavailable)?;
        let response = self
            .agent
            .post(&self.config.endpoint)
            .header("Accept", "application/json")
            .header("Authorization", authorization_text)
            .header("Content-Type", "application/json")
            .send(body);
        drop(authorization);
        drop(credential);
        let response = response.map_err(|_| SecretStoreError::unavailable())?;
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
            return Err(SecretStoreError::corrupt());
        }
        let mut reader = response.into_body().into_reader();
        let mut bytes = Vec::new();
        reader
            .by_ref()
            .take(
                u64::try_from(self.config.max_response_bytes)
                    .map_err(|_| SecretStoreError::corrupt())?
                    + 1,
            )
            .read_to_end(&mut bytes)
            .map_err(|_| SecretStoreError::unavailable())?;
        if bytes.is_empty() || bytes.len() > self.config.max_response_bytes {
            return Err(SecretStoreError::corrupt());
        }
        Ok(SensitiveBytes(bytes))
    }
}

impl SecretStorePort for VaultKmsNetworkAdapter {
    fn resolve(
        &self,
        reference: &CredentialReferenceResolution,
    ) -> Result<ResolvedSecret, SecretStoreError> {
        self.resolve_lease(reference)
            .map(VaultKmsNetworkLeasedSecret::into_secret)
    }
}

impl fmt::Debug for VaultKmsNetworkAdapter {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("VaultKmsNetworkAdapter")
            .field("config", &self.config)
            .field("identity", &"[REDACTED]")
            .finish_non_exhaustive()
    }
}

#[derive(Serialize)]
#[serde(tag = "operation", rename_all = "camelCase")]
enum ProtocolRequest<'a> {
    Resolve {
        schema: &'static str,
        operation_id: String,
        reference: &'a CredentialReferenceResolution,
    },
    Write {
        schema: &'static str,
        operation_id: String,
        reference: &'a CredentialReferenceResolution,
        rotation_version: u64,
        secret: SensitiveString,
    },
    Renew {
        schema: &'static str,
        operation_id: String,
        reference: &'a CredentialReferenceResolution,
        lease_id: &'a str,
    },
    Revoke {
        schema: &'static str,
        operation_id: String,
        reference: &'a CredentialReferenceResolution,
    },
    RotateKey {
        schema: &'static str,
        operation_id: String,
        key_version: u64,
    },
}

#[derive(Deserialize, Serialize)]
#[serde(tag = "result", rename_all = "camelCase", deny_unknown_fields)]
enum ProtocolResponse {
    Lease {
        schema: String,
        operation_id: String,
        rotation_version: u64,
        lease_id: String,
        issued_at_ms: u64,
        expires_at_ms: u64,
        secret: SensitiveString,
    },
    Written {
        schema: String,
        operation_id: String,
        rotation_version: u64,
        key_version: u64,
        replayed: bool,
    },
    Renewed {
        schema: String,
        operation_id: String,
        rotation_version: u64,
        lease_id: String,
        issued_at_ms: u64,
        expires_at_ms: u64,
    },
    Revoked {
        schema: String,
        operation_id: String,
        removed_versions: u64,
    },
    KeyRotated {
        schema: String,
        operation_id: String,
        key_version: u64,
        rewrapped_versions: u64,
    },
}

struct SensitiveString(Vec<u8>);

impl Serialize for SensitiveString {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let value = str::from_utf8(&self.0).map_err(serde::ser::Error::custom)?;
        serializer.serialize_str(value)
    }
}

impl<'de> Deserialize<'de> for SensitiveString {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct SensitiveStringVisitor;

        impl Visitor<'_> for SensitiveStringVisitor {
            type Value = SensitiveString;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a UTF-8 sensitive string")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok(SensitiveString(value.as_bytes().to_vec()))
            }
        }

        deserializer.deserialize_str(SensitiveStringVisitor)
    }
}

impl Drop for SensitiveString {
    fn drop(&mut self) {
        self.0.fill(0);
    }
}

struct SensitiveBytes(Vec<u8>);

impl Drop for SensitiveBytes {
    fn drop(&mut self) {
        self.0.fill(0);
    }
}

fn parse_response(
    bytes: &SensitiveBytes,
    max_response_bytes: usize,
) -> Result<ProtocolResponse, SecretStoreError> {
    if bytes.0.is_empty() || bytes.0.len() > max_response_bytes {
        return Err(SecretStoreError::corrupt());
    }
    let response: ProtocolResponse =
        serde_json::from_slice(&bytes.0).map_err(|_| SecretStoreError::corrupt())?;
    let canonical =
        SensitiveBytes(serde_json::to_vec(&response).map_err(|_| SecretStoreError::corrupt())?);
    if canonical.0 != bytes.0 {
        return Err(SecretStoreError::corrupt());
    }
    Ok(response)
}

fn validate_response_identity(
    schema: &str,
    expected_operation_id: &str,
    returned_operation_id: &str,
) -> Result<(), SecretStoreError> {
    if schema != RESPONSE_SCHEMA || returned_operation_id != expected_operation_id {
        return Err(SecretStoreError::corrupt());
    }
    Ok(())
}

fn validate_lease(
    expected_rotation_version: u64,
    rotation_version: u64,
    lease_id: &str,
    issued_at_ms: u64,
    expires_at_ms: u64,
    observed_at_ms: u64,
) -> Result<(), SecretStoreError> {
    if rotation_version != expected_rotation_version
        || !valid_token(lease_id, 200)
        || issued_at_ms == 0
        || issued_at_ms > observed_at_ms
        || expires_at_ms <= issued_at_ms
        || expires_at_ms <= observed_at_ms
        || expires_at_ms.saturating_sub(issued_at_ms) > MAX_WORKLOAD_CREDENTIAL_TTL_MS
    {
        return Err(SecretStoreError::corrupt());
    }
    Ok(())
}

fn operation_id(
    operation: &str,
    reference: &CredentialReferenceResolution,
    rotation_version: u64,
) -> Result<String, SecretStoreError> {
    let mut entropy = [0_u8; 16];
    getrandom::fill(&mut entropy).map_err(|_| SecretStoreError::unavailable())?;
    let reference = serde_json::to_vec(reference).map_err(|_| SecretStoreError::corrupt())?;
    let mut digest = Sha256::new();
    digest.update(b"winwincode.vault-kms-network-operation.v1\0");
    digest.update(operation.as_bytes());
    digest.update(rotation_version.to_be_bytes());
    digest.update(&reference);
    digest.update(entropy);
    entropy.fill(0);
    Ok(format!(
        "vko_{}",
        lower_hex(&digest.finalize())[..32].to_owned()
    ))
}

fn key_operation_id(key_version: u64) -> Result<String, SecretStoreError> {
    let mut entropy = [0_u8; 16];
    getrandom::fill(&mut entropy).map_err(|_| SecretStoreError::unavailable())?;
    let mut digest = Sha256::new();
    digest.update(b"winwincode.vault-kms-network-key-operation.v1\0");
    digest.update(key_version.to_be_bytes());
    digest.update(entropy);
    entropy.fill(0);
    Ok(format!(
        "vko_{}",
        lower_hex(&digest.finalize())[..32].to_owned()
    ))
}

fn status_error(status: u16) -> SecretStoreError {
    match status {
        404 | 410 => SecretStoreError::missing(),
        409 => SecretStoreError::version_conflict(),
        400 | 422 => SecretStoreError::corrupt(),
        _ => SecretStoreError::unavailable(),
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
        && value.len() <= "Bearer ".len() + MAX_TOKEN_BYTES
        && value.trim() == value
        && !value.chars().any(char::is_control)
}

fn valid_token(value: &str, max_len: usize) -> bool {
    !value.is_empty()
        && value.len() <= max_len
        && value.trim() == value
        && !value.chars().any(char::is_control)
}

fn lower_hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(char::from(DIGITS[usize::from(byte >> 4)]));
        output.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    output
}
