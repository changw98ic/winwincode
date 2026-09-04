// SPDX-License-Identifier: Apache-2.0

//! Verified S3-compatible byte storage for the canonical Artifact catalog.
//!
//! Artifact metadata, tenant authorization, retention, and upload ordering stay
//! in [`winwincode_storage::ArtifactStore`]. This crate owns only remote bytes,
//! a bounded HTTPS transport, and the secret-free object inventory used by the
//! canonical backup manifest.

use std::{fmt, io::Read as _, str, sync::Arc, time::Duration};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use winwincode_audit::AuditScope;
use winwincode_backup::{
    BackupComponentKind, BackupComponentSnapshot, BackupSnapshotRequest, BackupSnapshotSource,
    BackupSnapshotSourceError,
};
use winwincode_domain::{ArtifactId, Sha256Digest};
use winwincode_storage::{
    ArtifactError, ArtifactErrorKind, ArtifactObjectRange, ArtifactObjectStore,
    MAX_ARTIFACT_RANGE_BYTES,
};

const INVENTORY_SCHEMA: &str = "winwincode.s3-artifact-inventory.v1";
const MAX_ENDPOINT_BYTES: usize = 2_048;
const MAX_BUCKET_BYTES: usize = 63;
const MAX_PREFIX_BYTES: usize = 512;
const MAX_TOKEN_BYTES: usize = 16 * 1024;
const MAX_PART_BYTES: usize = 64 * 1024 * 1024;
const MAX_OBJECT_BYTES: usize = 2 * 1024 * 1024 * 1024;
const MAX_CONTROL_RESPONSE_BYTES: usize = 1024 * 1024;
const MAX_ATTEMPTS: u8 = 3;
const MAX_WORKLOAD_CREDENTIAL_TTL_MS: u64 = 15 * 60 * 1000;
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

/// TLS trust roots for one S3-compatible endpoint.
#[derive(Clone)]
pub enum S3ArtifactTlsRoots {
    /// Mozilla `WebPKI` roots shipped by the pinned HTTP stack.
    WebPki,
    /// Explicit DER roots for a private deployment or reproducible TLS gate.
    Specific(Vec<Vec<u8>>),
}

impl fmt::Debug for S3ArtifactTlsRoots {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WebPki => formatter.write_str("S3ArtifactTlsRoots::WebPki"),
            Self::Specific(roots) => formatter
                .debug_tuple("S3ArtifactTlsRoots::Specific")
                .field(&roots.len())
                .finish(),
        }
    }
}

/// Bounded deadlines for one S3-compatible request.
#[derive(Clone, Copy, Debug)]
pub struct S3ArtifactTimeouts {
    pub connect: Duration,
    pub response: Duration,
    pub total: Duration,
}

/// Verified HTTPS and capacity limits for one object-storage deployment.
#[derive(Clone)]
pub struct S3ArtifactConfig {
    endpoint: String,
    bucket: String,
    prefix: String,
    encryption: S3ArtifactEncryption,
    timeouts: S3ArtifactTimeouts,
    max_part_bytes: usize,
    max_object_bytes: usize,
    max_control_response_bytes: usize,
    max_attempts: u8,
    tls_roots: S3ArtifactTlsRoots,
}

/// Grouped capacity and retry limits for [`S3ArtifactConfig`].
#[derive(Clone, Copy, Debug)]
pub struct S3ArtifactLimits {
    pub max_part_bytes: usize,
    pub max_object_bytes: usize,
    pub max_control_response_bytes: usize,
    pub max_attempts: u8,
}

/// Required server-side KMS encryption facts for every accepted object.
#[derive(Clone, Eq, PartialEq)]
pub struct S3ArtifactEncryption {
    key_reference: String,
    context_digest: Sha256Digest,
}

impl S3ArtifactEncryption {
    /// Binds the adapter to one remote KMS key reference and secret-free
    /// encryption-context digest.
    ///
    /// # Errors
    ///
    /// Rejects empty/control-bearing key references or malformed digests.
    pub fn try_new(
        key_reference: String,
        context_digest: Sha256Digest,
    ) -> Result<Self, ArtifactError> {
        if key_reference.is_empty()
            || key_reference.len() > 512
            || key_reference.trim() != key_reference
            || key_reference.contains(['?', '#'])
            || key_reference.chars().any(char::is_control)
        {
            return Err(invalid("S3 KMS key reference is invalid"));
        }
        digest_hex(&context_digest)?;
        Ok(Self {
            key_reference,
            context_digest,
        })
    }
}

impl fmt::Debug for S3ArtifactEncryption {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("S3ArtifactEncryption")
            .field("key_reference", &"[REDACTED]")
            .field("context_digest", &"[BOUND]")
            .finish()
    }
}

impl S3ArtifactConfig {
    /// Creates a `WebPKI`-verified S3-compatible configuration.
    ///
    /// # Errors
    ///
    /// Rejects credential-bearing/non-HTTPS endpoints, unsafe bucket/prefix
    /// names, unbounded deadlines, object sizes, or retry counts.
    pub fn try_new(
        endpoint: String,
        bucket: String,
        prefix: String,
        encryption: S3ArtifactEncryption,
        timeouts: S3ArtifactTimeouts,
        limits: S3ArtifactLimits,
    ) -> Result<Self, ArtifactError> {
        let config = Self {
            endpoint,
            bucket,
            prefix,
            encryption,
            timeouts,
            max_part_bytes: limits.max_part_bytes,
            max_object_bytes: limits.max_object_bytes,
            max_control_response_bytes: limits.max_control_response_bytes,
            max_attempts: limits.max_attempts,
            tls_roots: S3ArtifactTlsRoots::WebPki,
        };
        config.validate()?;
        Ok(config)
    }

    /// Installs an explicit non-empty DER root set.
    ///
    /// # Errors
    ///
    /// Rejects empty or oversized root sets and certificates.
    pub fn with_specific_tls_roots(mut self, roots: Vec<Vec<u8>>) -> Result<Self, ArtifactError> {
        if roots.is_empty()
            || roots.len() > 32
            || roots
                .iter()
                .any(|root| root.is_empty() || root.len() > 64 * 1024)
        {
            return Err(invalid("S3 TLS root set is invalid"));
        }
        self.tls_roots = S3ArtifactTlsRoots::Specific(roots);
        self.validate()?;
        Ok(self)
    }

    fn validate(&self) -> Result<(), ArtifactError> {
        if !canonical_https_origin(&self.endpoint)
            || !canonical_bucket(&self.bucket)
            || !canonical_prefix(&self.prefix)
            || self.timeouts.connect.is_zero()
            || self.timeouts.response.is_zero()
            || self.timeouts.total.is_zero()
            || self.timeouts.connect > self.timeouts.total
            || self.timeouts.response > self.timeouts.total
            || self.max_part_bytes == 0
            || self.max_part_bytes > MAX_PART_BYTES
            || self.max_object_bytes == 0
            || self.max_object_bytes > MAX_OBJECT_BYTES
            || self.max_object_bytes < self.max_part_bytes
            || self.max_control_response_bytes == 0
            || self.max_control_response_bytes > MAX_CONTROL_RESPONSE_BYTES
            || self.max_attempts == 0
            || self.max_attempts > MAX_ATTEMPTS
        {
            return Err(invalid("S3 Artifact configuration is invalid"));
        }
        Ok(())
    }

    fn object_url(&self, digest: &Sha256Digest) -> Result<String, ArtifactError> {
        let hex = digest_hex(digest)?;
        Ok(format!(
            "{}/{}/{}/objects/sha256/{}/{}",
            self.endpoint,
            self.bucket,
            self.prefix,
            &hex[..2],
            &hex[2..]
        ))
    }

    fn upload_url(&self, artifact_id: &ArtifactId) -> Result<String, ArtifactError> {
        validate_artifact_id(artifact_id)?;
        Ok(format!(
            "{}/{}/{}/uploads/{}",
            self.endpoint, self.bucket, self.prefix, artifact_id.0
        ))
    }
}

impl fmt::Debug for S3ArtifactConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("S3ArtifactConfig")
            .field("endpoint", &"[REDACTED]")
            .field("bucket", &"[REDACTED]")
            .field("prefix", &"[REDACTED]")
            .field("encryption", &self.encryption)
            .field("timeouts", &self.timeouts)
            .field("max_part_bytes", &self.max_part_bytes)
            .field("max_object_bytes", &self.max_object_bytes)
            .field(
                "max_control_response_bytes",
                &self.max_control_response_bytes,
            )
            .field("max_attempts", &self.max_attempts)
            .field("tls_roots", &self.tls_roots)
            .finish()
    }
}

/// Wall-clock failure without endpoint or credential diagnostics.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct S3ArtifactClockError;

/// Clock used to validate short-lived workload credentials.
pub trait S3ArtifactClock: Send + Sync {
    /// Returns Unix time in milliseconds.
    ///
    /// # Errors
    ///
    /// Returns a secret-safe availability failure.
    fn now_ms(&self) -> Result<u64, S3ArtifactClockError>;
}

/// Production wall clock for object-storage identity expiry checks.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemS3ArtifactClock;

impl S3ArtifactClock for SystemS3ArtifactClock {
    fn now_ms(&self) -> Result<u64, S3ArtifactClockError> {
        let elapsed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| S3ArtifactClockError)?;
        u64::try_from(elapsed.as_millis()).map_err(|_| S3ArtifactClockError)
    }
}

/// One short-lived bearer identity acquired immediately before HTTPS.
pub struct S3ArtifactWorkloadCredential {
    token: SensitiveBytes,
    expires_at_ms: u64,
}

impl S3ArtifactWorkloadCredential {
    /// Owns a workload token and its authoritative expiry.
    ///
    /// # Errors
    ///
    /// Rejects empty/oversized tokens and an absent expiry.
    pub fn try_new(token: Vec<u8>, expires_at_ms: u64) -> Result<Self, ArtifactError> {
        if token.is_empty() || token.len() > MAX_TOKEN_BYTES || expires_at_ms == 0 {
            return Err(adapter_error());
        }
        Ok(Self {
            token: SensitiveBytes(token),
            expires_at_ms,
        })
    }

    #[must_use]
    pub const fn expires_at_ms(&self) -> u64 {
        self.expires_at_ms
    }
}

impl fmt::Debug for S3ArtifactWorkloadCredential {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("S3ArtifactWorkloadCredential")
            .field("token", &"[REDACTED]")
            .field("expires_at_ms", &self.expires_at_ms)
            .finish()
    }
}

/// Deployment identity source used once per S3-compatible request attempt.
pub trait S3ArtifactWorkloadIdentityPort: Send + Sync {
    /// Issues one bounded workload credential.
    ///
    /// # Errors
    ///
    /// Fails closed when workload identity is unavailable.
    fn issue(&self) -> Result<S3ArtifactWorkloadCredential, ArtifactError>;
}

/// Verified range result tied to the complete content address.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct S3ArtifactRangeRead {
    bytes: Vec<u8>,
    offset: u64,
    total_size: u64,
    digest: Sha256Digest,
}

impl S3ArtifactRangeRead {
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    #[must_use]
    pub const fn offset(&self) -> u64 {
        self.offset
    }

    #[must_use]
    pub const fn total_size(&self) -> u64 {
        self.total_size
    }

    #[must_use]
    pub const fn digest(&self) -> &Sha256Digest {
        &self.digest
    }
}

/// Secret-free, canonical remote object inventory.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct S3ArtifactInventoryReceipt {
    checkpoint_digest: Sha256Digest,
    content_digest: Sha256Digest,
    record_count: u64,
    byte_count: u64,
}

impl S3ArtifactInventoryReceipt {
    #[must_use]
    pub const fn checkpoint_digest(&self) -> &Sha256Digest {
        &self.checkpoint_digest
    }

    #[must_use]
    pub const fn content_digest(&self) -> &Sha256Digest {
        &self.content_digest
    }

    #[must_use]
    pub const fn record_count(&self) -> u64 {
        self.record_count
    }

    #[must_use]
    pub const fn byte_count(&self) -> u64 {
        self.byte_count
    }
}

/// Stateless verified-HTTPS S3-compatible implementation of the one
/// [`ArtifactObjectStore`] byte seam.
#[derive(Clone)]
pub struct S3ArtifactObjectStore {
    config: S3ArtifactConfig,
    agent: ureq::Agent,
    identity: Arc<dyn S3ArtifactWorkloadIdentityPort>,
    clock: Arc<dyn S3ArtifactClock>,
}

impl S3ArtifactObjectStore {
    /// Builds a no-proxy, no-redirect, rustls-verified object adapter.
    ///
    /// # Errors
    ///
    /// Rejects invalid network limits or trust roots.
    pub fn try_new(
        config: S3ArtifactConfig,
        identity: Arc<dyn S3ArtifactWorkloadIdentityPort>,
        clock: Arc<dyn S3ArtifactClock>,
    ) -> Result<Self, ArtifactError> {
        config.validate()?;
        let root_certs = match &config.tls_roots {
            S3ArtifactTlsRoots::WebPki => ureq::tls::RootCerts::WebPki,
            S3ArtifactTlsRoots::Specific(values) => values
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

    /// Creates the only `ArtifactObjects` backup source for this adapter.
    #[must_use]
    pub fn backup_source(&self, scope: AuditScope) -> S3ArtifactBackupSnapshotSource {
        S3ArtifactBackupSnapshotSource {
            store: self.clone(),
            scope,
        }
    }

    /// Reads and verifies one inclusive byte range from a complete object.
    ///
    /// # Errors
    ///
    /// Rejects malformed ranges, foreign/corrupt responses, or network errors.
    pub fn read_range(
        &self,
        digest: &Sha256Digest,
        offset: u64,
        length: u64,
    ) -> Result<Option<S3ArtifactRangeRead>, ArtifactError> {
        digest_hex(digest)?;
        if length == 0 || length > self.config.max_part_bytes as u64 {
            return Err(invalid("S3 Artifact range length is invalid"));
        }
        let end = offset
            .checked_add(length - 1)
            .filter(|value| *value <= MAX_SAFE_INTEGER)
            .ok_or_else(|| invalid("S3 Artifact range overflows"))?;
        let range = format!("bytes={offset}-{end}");
        let operation_id = operation_id("range", &[digest.0.as_bytes(), range.as_bytes()]);
        let url = self.config.object_url(digest)?;
        let response = self.execute(&WireRequest {
            method: WireMethod::Get,
            url: &url,
            operation_id: &operation_id,
            body: &[],
            content_type: None,
            checksum: None,
            range: Some(&range),
            max_response_bytes: self.config.max_part_bytes,
        })?;
        if response.status == 404 {
            require_one_of_statuses(&response, &[404], &operation_id)?;
            return Ok(None);
        }
        require_response_status(&response, 206, &operation_id)?;
        require_octet_stream(&response)?;
        require_encryption_receipt(&response, &self.config.encryption)?;
        if response.checksum.as_deref() != Some(digest.0.as_str())
            || response.body.len() as u64 != length
        {
            return Err(corrupt("S3 range response integrity failed"));
        }
        let total_size = parse_content_range(
            response
                .content_range
                .as_deref()
                .ok_or_else(|| corrupt("S3 range response has no Content-Range"))?,
            offset,
            end,
        )?;
        if total_size > self.config.max_object_bytes as u64 {
            return Err(corrupt("S3 range total size exceeds configured capacity"));
        }
        Ok(Some(S3ArtifactRangeRead {
            bytes: response.body,
            offset,
            total_size,
            digest: digest.clone(),
        }))
    }

    /// Idempotently removes one unfinished multipart upload.
    ///
    /// # Errors
    ///
    /// Rejects malformed identities or an unavailable/foreign endpoint.
    pub fn abort_upload(&self, artifact_id: &ArtifactId) -> Result<(), ArtifactError> {
        validate_artifact_id(artifact_id)?;
        let upload_id = upload_id(&self.config, artifact_id);
        let operation_id = operation_id("abort", &[artifact_id.0.as_bytes(), upload_id.as_bytes()]);
        let base = self.config.upload_url(artifact_id)?;
        let url = format!("{base}?uploadId={upload_id}");
        let response = self.execute(&WireRequest {
            method: WireMethod::Delete,
            url: &url,
            operation_id: &operation_id,
            body: &[],
            content_type: None,
            checksum: None,
            range: None,
            max_response_bytes: self.config.max_control_response_bytes,
        })?;
        require_one_of_statuses(&response, &[204, 404], &operation_id)
    }

    fn inventory(
        &self,
        consistency_cut_digest: &Sha256Digest,
        scope_digest: &Sha256Digest,
    ) -> Result<S3ArtifactInventoryReceipt, ArtifactError> {
        let cut_hex = digest_hex(consistency_cut_digest)?;
        let scope_hex = digest_hex(scope_digest)?;
        let operation_id = operation_id(
            "inventory",
            &[
                consistency_cut_digest.0.as_bytes(),
                scope_digest.0.as_bytes(),
                self.config.prefix.as_bytes(),
            ],
        );
        let url = format!(
            "{}/{}?list-type=2&prefix={}/objects/sha256/&consistency-cut={cut_hex}&scope-digest={scope_hex}",
            self.config.endpoint, self.config.bucket, self.config.prefix,
        );
        let response = self.execute(&WireRequest {
            method: WireMethod::Get,
            url: &url,
            operation_id: &operation_id,
            body: &[],
            content_type: None,
            checksum: None,
            range: None,
            max_response_bytes: self.config.max_control_response_bytes,
        })?;
        require_response_status(&response, 200, &operation_id)?;
        require_json(&response)?;
        let inventory: InventoryWire = serde_json::from_slice(&response.body)
            .map_err(|_| corrupt("S3 inventory response is invalid"))?;
        let canonical = serde_json::to_vec(&inventory)
            .map_err(|_| corrupt("S3 inventory response is invalid"))?;
        if canonical != response.body
            || inventory.schema != INVENTORY_SCHEMA
            || inventory.operation_id != operation_id
            || inventory.consistency_cut_digest != *consistency_cut_digest
            || inventory.scope_digest != *scope_digest
            || inventory.record_count > MAX_SAFE_INTEGER
            || inventory.byte_count > MAX_SAFE_INTEGER
        {
            return Err(corrupt("S3 inventory response is not canonical"));
        }
        digest_hex(&inventory.checkpoint_digest)?;
        digest_hex(&inventory.content_digest)?;
        Ok(S3ArtifactInventoryReceipt {
            checkpoint_digest: inventory.checkpoint_digest,
            content_digest: inventory.content_digest,
            record_count: inventory.record_count,
            byte_count: inventory.byte_count,
        })
    }

    fn execute(&self, request: &WireRequest<'_>) -> Result<WireResponse, ArtifactError> {
        for attempt in 1..=self.config.max_attempts {
            match self.send_once(request) {
                Ok(response) => return Ok(response),
                Err(error)
                    if error.kind() == ArtifactErrorKind::Adapter
                        && attempt < self.config.max_attempts => {}
                Err(error) => return Err(error),
            }
        }
        Err(adapter_error())
    }

    fn send_once(&self, request: &WireRequest<'_>) -> Result<WireResponse, ArtifactError> {
        let credential = self.identity.issue()?;
        let now_ms = self.clock.now_ms().map_err(|_| adapter_error())?;
        if credential.expires_at_ms <= now_ms
            || credential.expires_at_ms.saturating_sub(now_ms) > MAX_WORKLOAD_CREDENTIAL_TTL_MS
        {
            return Err(adapter_error());
        }
        let mut authorization = SensitiveBytes(Vec::with_capacity(
            "Bearer ".len() + credential.token.0.len(),
        ));
        authorization.0.extend_from_slice(b"Bearer ");
        authorization.0.extend_from_slice(&credential.token.0);
        let authorization_text = str::from_utf8(&authorization.0)
            .ok()
            .filter(|value| valid_header_value(value))
            .ok_or_else(adapter_error)?;
        let response = match request.method {
            WireMethod::Get => {
                let mut builder = self
                    .agent
                    .get(request.url)
                    .header("Accept", "application/octet-stream, application/json")
                    .header("Authorization", authorization_text)
                    .header("X-WinWinCode-Operation-Id", request.operation_id);
                if let Some(range) = request.range {
                    builder = builder.header("Range", range);
                }
                builder.call()
            }
            WireMethod::Put => self.send_with_body(request, authorization_text, true),
            WireMethod::Post => self.send_with_body(request, authorization_text, false),
            WireMethod::Delete => self
                .agent
                .delete(request.url)
                .header("Accept", "application/octet-stream")
                .header("Authorization", authorization_text)
                .header("X-WinWinCode-Operation-Id", request.operation_id)
                .call(),
        };
        drop(authorization);
        drop(credential);
        let response = response.map_err(|_| adapter_error())?;
        read_wire_response(response, request.max_response_bytes)
    }

    fn send_with_body(
        &self,
        request: &WireRequest<'_>,
        authorization: &str,
        put: bool,
    ) -> Result<ureq::http::Response<ureq::Body>, ureq::Error> {
        if put {
            let mut builder = self
                .agent
                .put(request.url)
                .header("Accept", "application/octet-stream")
                .header("Authorization", authorization)
                .header(
                    "Content-Type",
                    request.content_type.unwrap_or("application/octet-stream"),
                )
                .header("X-Amz-Server-Side-Encryption", "aws:kms")
                .header(
                    "X-Amz-Server-Side-Encryption-Aws-Kms-Key-Id",
                    &self.config.encryption.key_reference,
                )
                .header(
                    "X-Amz-Meta-WinWinCode-Encryption-Context-Sha256",
                    &self.config.encryption.context_digest.0,
                )
                .header("X-WinWinCode-Operation-Id", request.operation_id);
            if let Some(checksum) = request.checksum {
                builder = builder.header("X-Amz-Meta-WinWinCode-Sha256", checksum);
            }
            builder.send(request.body)
        } else {
            let mut builder = self
                .agent
                .post(request.url)
                .header("Accept", "application/octet-stream")
                .header("Authorization", authorization)
                .header(
                    "Content-Type",
                    request.content_type.unwrap_or("application/json"),
                )
                .header("X-Amz-Server-Side-Encryption", "aws:kms")
                .header(
                    "X-Amz-Server-Side-Encryption-Aws-Kms-Key-Id",
                    &self.config.encryption.key_reference,
                )
                .header(
                    "X-Amz-Meta-WinWinCode-Encryption-Context-Sha256",
                    &self.config.encryption.context_digest.0,
                )
                .header("X-WinWinCode-Operation-Id", request.operation_id);
            if let Some(checksum) = request.checksum {
                builder = builder.header("X-Amz-Meta-WinWinCode-Sha256", checksum);
            }
            builder.send(request.body)
        }
    }
}

impl ArtifactObjectStore for S3ArtifactObjectStore {
    fn put_chunk(
        &mut self,
        artifact_id: &ArtifactId,
        sequence: u64,
        digest: &Sha256Digest,
        bytes: &[u8],
    ) -> Result<(), ArtifactError> {
        validate_artifact_id(artifact_id)?;
        let expected = digest_hex(digest)?;
        if sequence == 0
            || sequence > MAX_SAFE_INTEGER
            || bytes.is_empty()
            || bytes.len() > self.config.max_part_bytes
            || lower_hex(&Sha256::digest(bytes)) != expected
        {
            return Err(ArtifactError::object_adapter(
                ArtifactErrorKind::DigestMismatch,
            ));
        }
        let upload_id = upload_id(&self.config, artifact_id);
        let operation_id = operation_id(
            "put-part",
            &[
                artifact_id.0.as_bytes(),
                &sequence.to_be_bytes(),
                digest.0.as_bytes(),
            ],
        );
        let base = self.config.upload_url(artifact_id)?;
        let url = format!("{base}?partNumber={sequence}&uploadId={upload_id}");
        let response = self.execute(&WireRequest {
            method: WireMethod::Put,
            url: &url,
            operation_id: &operation_id,
            body: bytes,
            content_type: Some("application/octet-stream"),
            checksum: Some(&digest.0),
            range: None,
            max_response_bytes: self.config.max_control_response_bytes,
        })?;
        require_empty_status(&response, 200, &operation_id)?;
        require_encryption_receipt(&response, &self.config.encryption)?;
        if response.checksum.as_deref() != Some(digest.0.as_str()) {
            return Err(corrupt("S3 multipart response checksum is invalid"));
        }
        Ok(())
    }

    fn finalize(
        &mut self,
        artifact_id: &ArtifactId,
        last_sequence: u64,
        digest: &Sha256Digest,
        size_bytes: u64,
    ) -> Result<(), ArtifactError> {
        validate_artifact_id(artifact_id)?;
        digest_hex(digest)?;
        if last_sequence == 0
            || last_sequence > MAX_SAFE_INTEGER
            || size_bytes > self.config.max_object_bytes as u64
        {
            return Err(invalid("S3 multipart completion facts are invalid"));
        }
        let upload_id = upload_id(&self.config, artifact_id);
        let body = serde_json::to_vec(&CompleteMultipartWire {
            digest,
            last_sequence,
            size_bytes,
        })
        .map_err(|_| invalid("S3 multipart completion facts are invalid"))?;
        let operation_id = operation_id(
            "complete",
            &[
                artifact_id.0.as_bytes(),
                &last_sequence.to_be_bytes(),
                digest.0.as_bytes(),
                &size_bytes.to_be_bytes(),
            ],
        );
        let base = self.config.upload_url(artifact_id)?;
        let url = format!("{base}?uploadId={upload_id}");
        let response = self.execute(&WireRequest {
            method: WireMethod::Post,
            url: &url,
            operation_id: &operation_id,
            body: &body,
            content_type: Some("application/json"),
            checksum: Some(&digest.0),
            range: None,
            max_response_bytes: self.config.max_control_response_bytes,
        })?;
        require_empty_status(&response, 200, &operation_id)?;
        require_encryption_receipt(&response, &self.config.encryption)?;
        if response.checksum.as_deref() != Some(digest.0.as_str()) {
            return Err(corrupt("S3 completion response checksum is invalid"));
        }
        Ok(())
    }

    fn read(&self, digest: &Sha256Digest) -> Result<Option<Vec<u8>>, ArtifactError> {
        let expected = digest_hex(digest)?;
        let operation_id = operation_id("read", &[digest.0.as_bytes()]);
        let url = self.config.object_url(digest)?;
        let response = self.execute(&WireRequest {
            method: WireMethod::Get,
            url: &url,
            operation_id: &operation_id,
            body: &[],
            content_type: None,
            checksum: None,
            range: None,
            max_response_bytes: self.config.max_object_bytes,
        })?;
        if response.status == 404 {
            require_one_of_statuses(&response, &[404], &operation_id)?;
            return Ok(None);
        }
        require_response_status(&response, 200, &operation_id)?;
        require_octet_stream(&response)?;
        require_encryption_receipt(&response, &self.config.encryption)?;
        if response.checksum.as_deref() != Some(digest.0.as_str())
            || lower_hex(&Sha256::digest(&response.body)) != expected
        {
            return Err(ArtifactError::object_adapter(
                ArtifactErrorKind::DigestMismatch,
            ));
        }
        Ok(Some(response.body))
    }

    fn read_range(
        &self,
        digest: &Sha256Digest,
        size_bytes: u64,
        offset: u64,
        length: u64,
    ) -> Result<Option<ArtifactObjectRange>, ArtifactError> {
        digest_hex(digest)?;
        if length == 0
            || length > MAX_ARTIFACT_RANGE_BYTES
            || size_bytes > self.config.max_object_bytes as u64
        {
            return Err(invalid("S3 Artifact range authority is invalid"));
        }
        let requested_end = offset
            .checked_add(length)
            .ok_or_else(|| invalid("S3 Artifact range overflows"))?;
        if offset >= size_bytes {
            return Err(invalid("S3 Artifact range starts outside the object"));
        }
        let exact_length = requested_end.min(size_bytes) - offset;
        let Some(range) = S3ArtifactObjectStore::read_range(self, digest, offset, exact_length)?
        else {
            return Ok(None);
        };
        if range.offset() != offset
            || range.total_size() != size_bytes
            || range.digest() != digest
            || range.bytes().len() as u64 != exact_length
        {
            return Err(corrupt(
                "S3 Artifact range disagrees with catalog authority",
            ));
        }
        ArtifactObjectRange::verified(
            range.bytes().to_vec(),
            range.offset(),
            range.total_size(),
            range.digest().clone(),
        )
        .map(Some)
        .map_err(|_| corrupt("S3 Artifact range disagrees with catalog authority"))
    }

    fn delete(&mut self, digest: &Sha256Digest) -> Result<(), ArtifactError> {
        digest_hex(digest)?;
        let operation_id = operation_id("delete", &[digest.0.as_bytes()]);
        let url = self.config.object_url(digest)?;
        let response = self.execute(&WireRequest {
            method: WireMethod::Delete,
            url: &url,
            operation_id: &operation_id,
            body: &[],
            content_type: None,
            checksum: None,
            range: None,
            max_response_bytes: self.config.max_control_response_bytes,
        })?;
        require_one_of_statuses(&response, &[204, 404], &operation_id)
    }
}

impl fmt::Debug for S3ArtifactObjectStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("S3ArtifactObjectStore")
            .field("config", &self.config)
            .field("identity", &"[REDACTED]")
            .finish_non_exhaustive()
    }
}

/// `ArtifactObjects` adapter for the canonical secret-free backup manifest.
pub struct S3ArtifactBackupSnapshotSource {
    store: S3ArtifactObjectStore,
    scope: AuditScope,
}

impl BackupSnapshotSource for S3ArtifactBackupSnapshotSource {
    fn kind(&self) -> BackupComponentKind {
        BackupComponentKind::ArtifactObjects
    }

    fn snapshot(
        &mut self,
        request: &BackupSnapshotRequest,
    ) -> Result<BackupComponentSnapshot, BackupSnapshotSourceError> {
        if request.scope() != &self.scope {
            return Err(BackupSnapshotSourceError::new());
        }
        let scope_digest =
            scope_digest(&self.scope).map_err(|_| BackupSnapshotSourceError::new())?;
        let inventory = self
            .store
            .inventory(request.consistency_cut_digest(), &scope_digest)
            .map_err(|_| BackupSnapshotSourceError::new())?;
        BackupComponentSnapshot::try_new(
            BackupComponentKind::ArtifactObjects,
            self.scope.clone(),
            request.consistency_cut_digest().clone(),
            inventory.checkpoint_digest,
            inventory.content_digest,
            inventory.record_count,
            inventory.byte_count,
        )
        .map_err(|_| BackupSnapshotSourceError::new())
    }
}

impl fmt::Debug for S3ArtifactBackupSnapshotSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("S3ArtifactBackupSnapshotSource")
            .field("store", &"[REDACTED]")
            .field("scope", &"[BOUND]")
            .finish()
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CompleteMultipartWire<'a> {
    digest: &'a Sha256Digest,
    last_sequence: u64,
    size_bytes: u64,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct InventoryWire {
    schema: String,
    operation_id: String,
    consistency_cut_digest: Sha256Digest,
    scope_digest: Sha256Digest,
    checkpoint_digest: Sha256Digest,
    content_digest: Sha256Digest,
    record_count: u64,
    byte_count: u64,
}

#[derive(Clone, Copy)]
enum WireMethod {
    Get,
    Put,
    Post,
    Delete,
}

struct WireRequest<'a> {
    method: WireMethod,
    url: &'a str,
    operation_id: &'a str,
    body: &'a [u8],
    content_type: Option<&'static str>,
    checksum: Option<&'a str>,
    range: Option<&'a str>,
    max_response_bytes: usize,
}

struct WireResponse {
    status: u16,
    body: Vec<u8>,
    content_type: Option<String>,
    operation_id: Option<String>,
    checksum: Option<String>,
    content_range: Option<String>,
    server_side_encryption: Option<String>,
    kms_key_reference: Option<String>,
}

struct SensitiveBytes(Vec<u8>);

impl Drop for SensitiveBytes {
    fn drop(&mut self) {
        self.0.fill(0);
    }
}

fn read_wire_response(
    response: ureq::http::Response<ureq::Body>,
    maximum: usize,
) -> Result<WireResponse, ArtifactError> {
    let status = response.status().as_u16();
    let content_type = response_header(&response, "content-type")?;
    let operation_id = response_header(&response, "x-winwincode-operation-id")?;
    let checksum = response_header(&response, "x-amz-meta-winwincode-sha256")?;
    let content_range = response_header(&response, "content-range")?;
    let server_side_encryption = response_header(&response, "x-amz-server-side-encryption")?;
    let kms_key_reference =
        response_header(&response, "x-amz-server-side-encryption-aws-kms-key-id")?;
    let mut reader = response.into_body().into_reader();
    let mut body = Vec::new();
    reader
        .by_ref()
        .take(
            u64::try_from(maximum)
                .map_err(|_| corrupt("S3 response limit is invalid"))?
                .saturating_add(1),
        )
        .read_to_end(&mut body)
        .map_err(|_| adapter_error())?;
    if body.len() > maximum {
        return Err(corrupt("S3 response exceeds the configured bound"));
    }
    Ok(WireResponse {
        status,
        body,
        content_type,
        operation_id,
        checksum,
        content_range,
        server_side_encryption,
        kms_key_reference,
    })
}

fn response_header(
    response: &ureq::http::Response<ureq::Body>,
    name: &str,
) -> Result<Option<String>, ArtifactError> {
    response
        .headers()
        .get(name)
        .map(|value| {
            value
                .to_str()
                .ok()
                .filter(|value| value.len() <= 1_024 && !value.chars().any(char::is_control))
                .map(str::to_owned)
                .ok_or_else(|| corrupt("S3 response header is invalid"))
        })
        .transpose()
}

fn require_response_status(
    response: &WireResponse,
    expected: u16,
    operation_id: &str,
) -> Result<(), ArtifactError> {
    if response.status != expected {
        return Err(status_error(response.status));
    }
    if response.operation_id.as_deref() != Some(operation_id) {
        return Err(corrupt("S3 operation identity is invalid"));
    }
    Ok(())
}

fn require_empty_status(
    response: &WireResponse,
    expected: u16,
    operation_id: &str,
) -> Result<(), ArtifactError> {
    require_one_of_statuses(response, &[expected], operation_id)
}

fn require_one_of_statuses(
    response: &WireResponse,
    expected: &[u16],
    operation_id: &str,
) -> Result<(), ArtifactError> {
    if !expected.contains(&response.status) {
        return Err(status_error(response.status));
    }
    if response.operation_id.as_deref() != Some(operation_id) || !response.body.is_empty() {
        return Err(corrupt("S3 operation receipt is invalid"));
    }
    Ok(())
}

fn require_octet_stream(response: &WireResponse) -> Result<(), ArtifactError> {
    if !response
        .content_type
        .as_deref()
        .is_some_and(|value| value.eq_ignore_ascii_case("application/octet-stream"))
    {
        return Err(corrupt("S3 object response content type is invalid"));
    }
    Ok(())
}

fn require_json(response: &WireResponse) -> Result<(), ArtifactError> {
    if !response
        .content_type
        .as_deref()
        .is_some_and(|value| value.eq_ignore_ascii_case("application/json"))
    {
        return Err(corrupt("S3 inventory response content type is invalid"));
    }
    Ok(())
}

fn require_encryption_receipt(
    response: &WireResponse,
    encryption: &S3ArtifactEncryption,
) -> Result<(), ArtifactError> {
    if !response
        .server_side_encryption
        .as_deref()
        .is_some_and(|value| value.eq_ignore_ascii_case("aws:kms"))
        || response.kms_key_reference.as_deref() != Some(&encryption.key_reference)
    {
        return Err(corrupt("S3 KMS encryption receipt is invalid"));
    }
    Ok(())
}

fn status_error(status: u16) -> ArtifactError {
    let kind = match status {
        400 | 422 => ArtifactErrorKind::InvalidInput,
        401 | 403 => ArtifactErrorKind::PermissionDenied,
        404 => ArtifactErrorKind::NotFound,
        409 => ArtifactErrorKind::Conflict,
        412 => ArtifactErrorKind::DigestMismatch,
        _ => ArtifactErrorKind::Adapter,
    };
    ArtifactError::object_adapter(kind)
}

fn parse_content_range(value: &str, offset: u64, end: u64) -> Result<u64, ArtifactError> {
    let (bounds, total) = value
        .strip_prefix("bytes ")
        .and_then(|rest| rest.split_once('/'))
        .ok_or_else(|| corrupt("S3 Content-Range is invalid"))?;
    let (start, returned_end) = bounds
        .split_once('-')
        .ok_or_else(|| corrupt("S3 Content-Range is invalid"))?;
    let start = start
        .parse::<u64>()
        .map_err(|_| corrupt("S3 Content-Range is invalid"))?;
    let returned_end = returned_end
        .parse::<u64>()
        .map_err(|_| corrupt("S3 Content-Range is invalid"))?;
    let total = total
        .parse::<u64>()
        .map_err(|_| corrupt("S3 Content-Range is invalid"))?;
    if start != offset || returned_end != end || total <= end || total > MAX_SAFE_INTEGER {
        return Err(corrupt("S3 Content-Range is inconsistent"));
    }
    Ok(total)
}

fn upload_id(config: &S3ArtifactConfig, artifact_id: &ArtifactId) -> String {
    let value = stable_hash(
        b"winwincode.s3-artifact-upload.v1",
        &[
            config.bucket.as_bytes(),
            config.prefix.as_bytes(),
            artifact_id.0.as_bytes(),
        ],
    );
    format!("wwcu_{}", &value[..32])
}

fn operation_id(operation: &str, fields: &[&[u8]]) -> String {
    let mut all = Vec::with_capacity(fields.len() + 1);
    all.push(operation.as_bytes());
    all.extend_from_slice(fields);
    let value = stable_hash(b"winwincode.s3-artifact-operation.v1", &all);
    format!("wwco_{}", &value[..32])
}

fn stable_hash(domain: &[u8], fields: &[&[u8]]) -> String {
    let mut digest = Sha256::new();
    digest.update(domain);
    digest.update([0]);
    for field in fields {
        digest.update(u64::try_from(field.len()).unwrap_or(u64::MAX).to_be_bytes());
        digest.update(field);
    }
    lower_hex(&digest.finalize())
}

fn scope_digest(scope: &AuditScope) -> Result<Sha256Digest, ArtifactError> {
    let encoded = serde_json::to_vec(scope).map_err(|_| invalid("backup scope is invalid"))?;
    Ok(Sha256Digest(format!(
        "sha256:{}",
        stable_hash(b"winwincode.s3-artifact-backup-scope.v1", &[&encoded])
    )))
}

fn digest_hex(digest: &Sha256Digest) -> Result<&str, ArtifactError> {
    digest
        .0
        .strip_prefix("sha256:")
        .filter(|value| {
            value.len() == 64
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
        })
        .ok_or_else(|| invalid("Artifact digest is not canonical SHA-256"))
}

fn validate_artifact_id(artifact_id: &ArtifactId) -> Result<(), ArtifactError> {
    let valid = artifact_id
        .0
        .strip_prefix("art_")
        .is_some_and(|suffix| suffix.len() == 26 && suffix.bytes().all(crockford_byte));
    if valid {
        Ok(())
    } else {
        Err(invalid("Artifact identity is not canonical"))
    }
}

fn crockford_byte(byte: u8) -> bool {
    byte.is_ascii_digit()
        || matches!(
            byte,
            b'A'..=b'H' | b'J' | b'K' | b'M' | b'N' | b'P'..=b'T' | b'V'..=b'Z'
        )
}

fn canonical_https_origin(value: &str) -> bool {
    value.len() <= MAX_ENDPOINT_BYTES
        && value.starts_with("https://")
        && value.trim() == value
        && !value.ends_with('/')
        && !value.contains(['?', '#'])
        && !value.chars().any(char::is_control)
        && value
            .strip_prefix("https://")
            .is_some_and(|authority| !authority.is_empty() && !authority.contains(['/', '@']))
}

fn canonical_bucket(value: &str) -> bool {
    (3..=MAX_BUCKET_BYTES).contains(&value.len())
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'-')
        })
        && value
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        && value
            .bytes()
            .last()
            .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        && !value.contains("..")
}

fn canonical_prefix(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_PREFIX_BYTES
        && value.trim_matches('/') == value
        && value.split('/').all(|part| {
            !part.is_empty()
                && part != "."
                && part != ".."
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        })
}

fn valid_header_value(value: &str) -> bool {
    value.len() > "Bearer ".len()
        && value.len() <= "Bearer ".len() + MAX_TOKEN_BYTES
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

fn invalid(_message: &'static str) -> ArtifactError {
    ArtifactError::object_adapter(ArtifactErrorKind::InvalidInput)
}

fn corrupt(_message: &'static str) -> ArtifactError {
    ArtifactError::object_adapter(ArtifactErrorKind::Corrupt)
}

fn adapter_error() -> ArtifactError {
    ArtifactError::object_adapter(ArtifactErrorKind::Adapter)
}
