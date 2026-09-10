// SPDX-License-Identifier: Apache-2.0

//! Product-neutral S3-compatible Artifact byte transport.
//!
//! Deployment routing, credentials, and encryption policy are supplied for each
//! request by the composing product. This crate retains no bucket, prefix,
//! region, key, proxy, or retention configuration.

use std::{collections::BTreeSet, fmt, io::Read as _, sync::Arc, time::Duration};

use serde::Serialize;
use sha2::{Digest, Sha256};
use winwincode_domain::{ArtifactId, Sha256Digest};

const MAX_URL_BYTES: usize = 4_096;
const MAX_HEADER_COUNT: usize = 64;
const MAX_HEADER_NAME_BYTES: usize = 64;
const MAX_HEADER_VALUE_BYTES: usize = 16 * 1_024;
const MAX_PART_BYTES: usize = 64 * 1_024 * 1_024;
const MAX_OBJECT_BYTES: usize = 2 * 1_024 * 1_024 * 1_024;
const MAX_CONTROL_RESPONSE_BYTES: usize = 1_024 * 1_024;
const MAX_ATTEMPTS: u8 = 3;
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

/// Stable S3 adapter failure categories.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum S3ArtifactErrorKind {
    Invalid,
    NotFound,
    Conflict,
    PermissionDenied,
    DigestMismatch,
    Transport,
    CorruptResponse,
}

/// Secret-safe S3 adapter failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct S3ArtifactError {
    kind: S3ArtifactErrorKind,
    message: &'static str,
}

impl S3ArtifactError {
    const fn new(kind: S3ArtifactErrorKind, message: &'static str) -> Self {
        Self { kind, message }
    }

    #[must_use]
    pub const fn kind(&self) -> S3ArtifactErrorKind {
        self.kind
    }
}

impl fmt::Display for S3ArtifactError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message)
    }
}

impl std::error::Error for S3ArtifactError {}

/// TLS trust roots for one S3-compatible endpoint.
#[derive(Clone)]
pub enum S3ArtifactTlsRoots {
    WebPki,
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

/// Bounded request deadlines.
#[derive(Clone, Copy, Debug)]
pub struct S3ArtifactTimeouts {
    pub connect: Duration,
    pub response: Duration,
    pub total: Duration,
}

/// Bounded multipart, response, and retry limits.
#[derive(Clone, Copy, Debug)]
pub struct S3ArtifactLimits {
    pub max_part_bytes: usize,
    pub max_object_bytes: usize,
    pub max_control_response_bytes: usize,
    pub max_attempts: u8,
}

/// Product-neutral HTTPS transport policy.
#[derive(Clone, Debug)]
pub struct S3ArtifactTransportConfig {
    timeouts: S3ArtifactTimeouts,
    limits: S3ArtifactLimits,
    tls_roots: S3ArtifactTlsRoots,
}

impl S3ArtifactTransportConfig {
    /// Creates a WebPKI-verified, no-proxy, no-redirect transport policy.
    ///
    /// # Errors
    ///
    /// Rejects zero, inconsistent, or excessive limits.
    pub fn try_new(
        timeouts: S3ArtifactTimeouts,
        limits: S3ArtifactLimits,
    ) -> Result<Self, S3ArtifactError> {
        let config = Self {
            timeouts,
            limits,
            tls_roots: S3ArtifactTlsRoots::WebPki,
        };
        config.validate()?;
        Ok(config)
    }

    /// Installs an explicit non-empty DER trust-root set.
    ///
    /// # Errors
    ///
    /// Rejects empty or oversized root sets and certificates.
    pub fn with_specific_tls_roots(mut self, roots: Vec<Vec<u8>>) -> Result<Self, S3ArtifactError> {
        if roots.is_empty()
            || roots.len() > 32
            || roots
                .iter()
                .any(|root| root.is_empty() || root.len() > 64 * 1_024)
        {
            return Err(invalid());
        }
        self.tls_roots = S3ArtifactTlsRoots::Specific(roots);
        self.validate()?;
        Ok(self)
    }

    fn validate(&self) -> Result<(), S3ArtifactError> {
        if self.timeouts.connect.is_zero()
            || self.timeouts.response.is_zero()
            || self.timeouts.total.is_zero()
            || self.timeouts.connect > self.timeouts.total
            || self.timeouts.response > self.timeouts.total
            || self.limits.max_part_bytes == 0
            || self.limits.max_part_bytes > MAX_PART_BYTES
            || self.limits.max_object_bytes == 0
            || self.limits.max_object_bytes > MAX_OBJECT_BYTES
            || self.limits.max_object_bytes < self.limits.max_part_bytes
            || self.limits.max_control_response_bytes == 0
            || self.limits.max_control_response_bytes > MAX_CONTROL_RESPONSE_BYTES
            || self.limits.max_attempts == 0
            || self.limits.max_attempts > MAX_ATTEMPTS
        {
            return Err(invalid());
        }
        Ok(())
    }
}

/// S3-compatible HTTP method exposed to request-policy ports.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum S3ArtifactMethod {
    Get,
    Put,
    Post,
    Delete,
}

impl S3ArtifactMethod {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Put => "PUT",
            Self::Post => "POST",
            Self::Delete => "DELETE",
        }
    }
}

/// Validated request headers returned by an injected policy port.
#[derive(Clone, Default)]
pub struct S3ArtifactHeaders(Vec<(String, String)>);

impl S3ArtifactHeaders {
    /// Validates bounded headers without retaining them in the adapter.
    ///
    /// # Errors
    ///
    /// Rejects duplicate, reserved, malformed, or unbounded headers.
    pub fn try_new(values: Vec<(String, String)>) -> Result<Self, S3ArtifactError> {
        if values.len() > MAX_HEADER_COUNT {
            return Err(invalid());
        }
        let mut names = BTreeSet::new();
        for (name, value) in &values {
            if !valid_header_name(name)
                || reserved_header(name)
                || !valid_header_value(value)
                || !names.insert(name.as_str())
            {
                return Err(invalid());
            }
        }
        Ok(Self(values))
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.0
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
    }
}

impl fmt::Debug for S3ArtifactHeaders {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("S3ArtifactHeaders")
            .field("count", &self.0.len())
            .finish()
    }
}

/// Immutable request facts used by identity and encryption policy ports.
pub struct S3ArtifactRequestContext<'a> {
    method: S3ArtifactMethod,
    url: &'a str,
    operation_id: &'a str,
    payload_sha256: &'a str,
    content_type: Option<&'static str>,
    range: Option<&'a str>,
    checksum: Option<&'a str>,
    policy_headers: &'a S3ArtifactHeaders,
}

impl S3ArtifactRequestContext<'_> {
    #[must_use]
    pub const fn method(&self) -> S3ArtifactMethod {
        self.method
    }

    #[must_use]
    pub const fn url(&self) -> &str {
        self.url
    }

    #[must_use]
    pub const fn operation_id(&self) -> &str {
        self.operation_id
    }

    #[must_use]
    pub const fn payload_sha256(&self) -> &str {
        self.payload_sha256
    }

    #[must_use]
    pub const fn content_type(&self) -> Option<&'static str> {
        self.content_type
    }

    #[must_use]
    pub const fn range(&self) -> Option<&str> {
        self.range
    }

    #[must_use]
    pub const fn checksum(&self) -> Option<&str> {
        self.checksum
    }

    #[must_use]
    pub const fn policy_headers(&self) -> &S3ArtifactHeaders {
        self.policy_headers
    }
}

/// Bounded response headers available to the encryption verifier.
pub struct S3ArtifactResponseHeaders(Vec<(String, String)>);

impl S3ArtifactResponseHeaders {
    #[must_use]
    pub fn value(&self, name: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(candidate, _)| candidate.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

impl fmt::Debug for S3ArtifactResponseHeaders {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("S3ArtifactResponseHeaders")
            .field("count", &self.0.len())
            .finish()
    }
}

/// Supplies per-attempt authorization headers without fixing a credential scheme.
pub trait S3ArtifactIdentityPort: Send + Sync {
    /// Authorizes one exact request after encryption headers are known.
    ///
    /// # Errors
    ///
    /// Fails closed when current identity cannot authorize the request.
    fn authorize(
        &self,
        request: &S3ArtifactRequestContext<'_>,
    ) -> Result<S3ArtifactHeaders, S3ArtifactError>;
}

/// Supplies and verifies deployment-owned encryption policy.
pub trait S3ArtifactEncryptionPort: Send + Sync {
    /// Returns encryption headers for one exact request.
    ///
    /// # Errors
    ///
    /// Fails closed when the deployment policy cannot authorize encryption.
    fn request_headers(
        &self,
        request: &S3ArtifactRequestContext<'_>,
    ) -> Result<S3ArtifactHeaders, S3ArtifactError>;

    /// Verifies the response against the same deployment encryption policy.
    ///
    /// # Errors
    ///
    /// Rejects a missing or mismatched encryption receipt.
    fn verify_response(
        &self,
        request: &S3ArtifactRequestContext<'_>,
        response: &S3ArtifactResponseHeaders,
    ) -> Result<(), S3ArtifactError>;
}

/// Verified byte range tied to the complete content address.
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

/// Stateless verified-HTTPS S3-compatible byte adapter.
#[derive(Clone)]
pub struct S3ArtifactAdapter {
    config: S3ArtifactTransportConfig,
    agent: ureq::Agent,
    identity: Arc<dyn S3ArtifactIdentityPort>,
    encryption: Arc<dyn S3ArtifactEncryptionPort>,
}

impl S3ArtifactAdapter {
    /// Builds a no-proxy, no-redirect, rustls-verified adapter.
    ///
    /// # Errors
    ///
    /// Rejects invalid network limits or trust roots.
    pub fn try_new(
        config: S3ArtifactTransportConfig,
        identity: Arc<dyn S3ArtifactIdentityPort>,
        encryption: Arc<dyn S3ArtifactEncryptionPort>,
    ) -> Result<Self, S3ArtifactError> {
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
            encryption,
        })
    }

    /// Uploads one idempotent multipart chunk.
    ///
    /// # Errors
    ///
    /// Rejects invalid facts, changed replay, authorization failure, or transport failure.
    pub fn put_chunk(
        &self,
        upload_url: &str,
        artifact_id: &ArtifactId,
        sequence: u64,
        digest: &Sha256Digest,
        bytes: &[u8],
    ) -> Result<(), S3ArtifactError> {
        validate_base_url(upload_url)?;
        validate_artifact_id(artifact_id)?;
        let expected = digest_hex(digest)?;
        if sequence == 0
            || sequence > MAX_SAFE_INTEGER
            || bytes.is_empty()
            || bytes.len() > self.config.limits.max_part_bytes
            || lower_hex(&Sha256::digest(bytes)) != expected
        {
            return Err(S3ArtifactError::new(
                S3ArtifactErrorKind::DigestMismatch,
                "S3 Artifact chunk digest does not match",
            ));
        }
        let upload_id = upload_id(artifact_id);
        let operation_id = operation_id(
            "put-part",
            &[
                artifact_id.0.as_bytes(),
                &sequence.to_be_bytes(),
                digest.0.as_bytes(),
            ],
        );
        let url = format!("{upload_url}?partNumber={sequence}&uploadId={upload_id}");
        let request = WireRequest {
            method: S3ArtifactMethod::Put,
            url: &url,
            operation_id: &operation_id,
            body: bytes,
            content_type: Some("application/octet-stream"),
            checksum: Some(&digest.0),
            range: None,
            max_response_bytes: self.config.limits.max_control_response_bytes,
        };
        let response = self.execute(&request)?;
        require_empty_status(&response, 200, &operation_id)?;
        self.verify_encryption(&request, &response)?;
        if response.checksum.as_deref() != Some(digest.0.as_str()) {
            return Err(corrupt());
        }
        Ok(())
    }

    /// Completes one multipart upload and binds the full digest and size.
    ///
    /// # Errors
    ///
    /// Rejects invalid facts, a corrupt receipt, authorization failure, or transport failure.
    pub fn finalize(
        &self,
        upload_url: &str,
        artifact_id: &ArtifactId,
        last_sequence: u64,
        digest: &Sha256Digest,
        size_bytes: u64,
    ) -> Result<(), S3ArtifactError> {
        validate_base_url(upload_url)?;
        validate_artifact_id(artifact_id)?;
        digest_hex(digest)?;
        if last_sequence == 0
            || last_sequence > MAX_SAFE_INTEGER
            || size_bytes > self.config.limits.max_object_bytes as u64
        {
            return Err(invalid());
        }
        let upload_id = upload_id(artifact_id);
        let body = serde_json::to_vec(&CompleteMultipartWire {
            digest,
            last_sequence,
            size_bytes,
        })
        .map_err(|_| invalid())?;
        let operation_id = operation_id(
            "complete",
            &[
                artifact_id.0.as_bytes(),
                &last_sequence.to_be_bytes(),
                digest.0.as_bytes(),
                &size_bytes.to_be_bytes(),
            ],
        );
        let url = format!("{upload_url}?uploadId={upload_id}");
        let request = WireRequest {
            method: S3ArtifactMethod::Post,
            url: &url,
            operation_id: &operation_id,
            body: &body,
            content_type: Some("application/json"),
            checksum: Some(&digest.0),
            range: None,
            max_response_bytes: self.config.limits.max_control_response_bytes,
        };
        let response = self.execute(&request)?;
        require_empty_status(&response, 200, &operation_id)?;
        self.verify_encryption(&request, &response)?;
        if response.checksum.as_deref() != Some(digest.0.as_str()) {
            return Err(corrupt());
        }
        Ok(())
    }

    /// Reads and verifies one complete object.
    ///
    /// # Errors
    ///
    /// Rejects an invalid URL/digest, authorization failure, corrupt bytes, or transport failure.
    pub fn read(
        &self,
        object_url: &str,
        digest: &Sha256Digest,
    ) -> Result<Option<Vec<u8>>, S3ArtifactError> {
        validate_base_url(object_url)?;
        let expected = digest_hex(digest)?;
        let operation_id = operation_id("read", &[digest.0.as_bytes()]);
        let request = WireRequest {
            method: S3ArtifactMethod::Get,
            url: object_url,
            operation_id: &operation_id,
            body: &[],
            content_type: None,
            checksum: None,
            range: None,
            max_response_bytes: self.config.limits.max_object_bytes,
        };
        let response = self.execute(&request)?;
        if response.status == 404 {
            require_one_of_statuses(&response, &[404], &operation_id)?;
            return Ok(None);
        }
        require_response_status(&response, 200, &operation_id)?;
        require_octet_stream(&response)?;
        self.verify_encryption(&request, &response)?;
        if response.checksum.as_deref() != Some(digest.0.as_str())
            || lower_hex(&Sha256::digest(&response.body)) != expected
        {
            return Err(S3ArtifactError::new(
                S3ArtifactErrorKind::DigestMismatch,
                "S3 Artifact object digest does not match",
            ));
        }
        Ok(Some(response.body))
    }

    /// Reads and verifies one inclusive byte range.
    ///
    /// # Errors
    ///
    /// Rejects malformed ranges, authorization failure, corrupt bytes, or transport failure.
    pub fn read_range(
        &self,
        object_url: &str,
        digest: &Sha256Digest,
        offset: u64,
        length: u64,
    ) -> Result<Option<S3ArtifactRangeRead>, S3ArtifactError> {
        validate_base_url(object_url)?;
        digest_hex(digest)?;
        if length == 0 || length > self.config.limits.max_part_bytes as u64 {
            return Err(invalid());
        }
        let end = offset
            .checked_add(length - 1)
            .filter(|value| *value <= MAX_SAFE_INTEGER)
            .ok_or_else(invalid)?;
        let range = format!("bytes={offset}-{end}");
        let operation_id = operation_id("range", &[digest.0.as_bytes(), range.as_bytes()]);
        let request = WireRequest {
            method: S3ArtifactMethod::Get,
            url: object_url,
            operation_id: &operation_id,
            body: &[],
            content_type: None,
            checksum: None,
            range: Some(&range),
            max_response_bytes: self.config.limits.max_part_bytes,
        };
        let response = self.execute(&request)?;
        if response.status == 404 {
            require_one_of_statuses(&response, &[404], &operation_id)?;
            return Ok(None);
        }
        require_response_status(&response, 206, &operation_id)?;
        require_octet_stream(&response)?;
        self.verify_encryption(&request, &response)?;
        if response.checksum.as_deref() != Some(digest.0.as_str())
            || response.body.len() as u64 != length
        {
            return Err(corrupt());
        }
        let total_size = parse_content_range(
            response.content_range.as_deref().ok_or_else(corrupt)?,
            offset,
            end,
        )?;
        if total_size > self.config.limits.max_object_bytes as u64 {
            return Err(corrupt());
        }
        Ok(Some(S3ArtifactRangeRead {
            bytes: response.body,
            offset,
            total_size,
            digest: digest.clone(),
        }))
    }

    /// Idempotently removes one complete object.
    ///
    /// # Errors
    ///
    /// Rejects invalid routing, authorization failure, or transport failure.
    pub fn delete(&self, object_url: &str, digest: &Sha256Digest) -> Result<(), S3ArtifactError> {
        validate_base_url(object_url)?;
        digest_hex(digest)?;
        let operation_id = operation_id("delete", &[digest.0.as_bytes()]);
        let response = self.execute(&WireRequest {
            method: S3ArtifactMethod::Delete,
            url: object_url,
            operation_id: &operation_id,
            body: &[],
            content_type: None,
            checksum: None,
            range: None,
            max_response_bytes: self.config.limits.max_control_response_bytes,
        })?;
        require_one_of_statuses(&response, &[204, 404], &operation_id)
    }

    /// Idempotently removes one unfinished multipart upload.
    ///
    /// # Errors
    ///
    /// Rejects invalid routing/identity, authorization failure, or transport failure.
    pub fn abort_upload(
        &self,
        upload_url: &str,
        artifact_id: &ArtifactId,
    ) -> Result<(), S3ArtifactError> {
        validate_base_url(upload_url)?;
        validate_artifact_id(artifact_id)?;
        let upload_id = upload_id(artifact_id);
        let operation_id = operation_id("abort", &[artifact_id.0.as_bytes(), upload_id.as_bytes()]);
        let url = format!("{upload_url}?uploadId={upload_id}");
        let response = self.execute(&WireRequest {
            method: S3ArtifactMethod::Delete,
            url: &url,
            operation_id: &operation_id,
            body: &[],
            content_type: None,
            checksum: None,
            range: None,
            max_response_bytes: self.config.limits.max_control_response_bytes,
        })?;
        require_one_of_statuses(&response, &[204, 404], &operation_id)
    }

    fn execute(&self, request: &WireRequest<'_>) -> Result<WireResponse, S3ArtifactError> {
        for attempt in 1..=self.config.limits.max_attempts {
            match self.send_once(request) {
                Ok(response) => return Ok(response),
                Err(error)
                    if error.kind() == S3ArtifactErrorKind::Transport
                        && attempt < self.config.limits.max_attempts => {}
                Err(error) => return Err(error),
            }
        }
        Err(transport())
    }

    fn send_once(&self, request: &WireRequest<'_>) -> Result<WireResponse, S3ArtifactError> {
        let payload_sha256 = lower_hex(&Sha256::digest(request.body));
        let empty_headers = S3ArtifactHeaders::default();
        let base_context = request.context(&payload_sha256, &empty_headers);
        let encryption_headers = self.encryption.request_headers(&base_context)?;
        let context = request.context(&payload_sha256, &encryption_headers);
        let identity_headers = self.identity.authorize(&context)?;
        require_distinct_headers(&encryption_headers, &identity_headers)?;

        let response = match request.method {
            S3ArtifactMethod::Get => {
                let mut builder = self
                    .agent
                    .get(request.url)
                    .header("Accept", "application/octet-stream")
                    .header("X-WinWinCode-Operation-Id", request.operation_id);
                if let Some(range) = request.range {
                    builder = builder.header("Range", range);
                }
                for (name, value) in encryption_headers.iter().chain(identity_headers.iter()) {
                    builder = builder.header(name, value);
                }
                builder.call()
            }
            S3ArtifactMethod::Put => {
                self.send_with_body(request, &encryption_headers, &identity_headers, true)
            }
            S3ArtifactMethod::Post => {
                self.send_with_body(request, &encryption_headers, &identity_headers, false)
            }
            S3ArtifactMethod::Delete => {
                let mut builder = self
                    .agent
                    .delete(request.url)
                    .header("Accept", "application/octet-stream")
                    .header("X-WinWinCode-Operation-Id", request.operation_id);
                for (name, value) in encryption_headers.iter().chain(identity_headers.iter()) {
                    builder = builder.header(name, value);
                }
                builder.call()
            }
        }
        .map_err(|_| transport())?;
        read_wire_response(response, request.max_response_bytes, encryption_headers)
    }

    fn send_with_body(
        &self,
        request: &WireRequest<'_>,
        encryption_headers: &S3ArtifactHeaders,
        identity_headers: &S3ArtifactHeaders,
        put: bool,
    ) -> Result<ureq::http::Response<ureq::Body>, ureq::Error> {
        let content_type = request.content_type.unwrap_or("application/octet-stream");
        if put {
            let mut builder = self
                .agent
                .put(request.url)
                .header("Accept", "application/octet-stream")
                .header("Content-Type", content_type)
                .header("X-WinWinCode-Operation-Id", request.operation_id);
            if let Some(checksum) = request.checksum {
                builder = builder.header("X-Amz-Meta-WinWinCode-Sha256", checksum);
            }
            for (name, value) in encryption_headers.iter().chain(identity_headers.iter()) {
                builder = builder.header(name, value);
            }
            builder.send(request.body)
        } else {
            let mut builder = self
                .agent
                .post(request.url)
                .header("Accept", "application/octet-stream")
                .header("Content-Type", content_type)
                .header("X-WinWinCode-Operation-Id", request.operation_id);
            if let Some(checksum) = request.checksum {
                builder = builder.header("X-Amz-Meta-WinWinCode-Sha256", checksum);
            }
            for (name, value) in encryption_headers.iter().chain(identity_headers.iter()) {
                builder = builder.header(name, value);
            }
            builder.send(request.body)
        }
    }

    fn verify_encryption(
        &self,
        request: &WireRequest<'_>,
        response: &WireResponse,
    ) -> Result<(), S3ArtifactError> {
        let payload_sha256 = lower_hex(&Sha256::digest(request.body));
        let context = request.context(&payload_sha256, &response.policy_headers);
        self.encryption.verify_response(&context, &response.headers)
    }
}

impl fmt::Debug for S3ArtifactAdapter {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("S3ArtifactAdapter")
            .field("config", &self.config)
            .field("identity", &"[INJECTED]")
            .field("encryption", &"[INJECTED]")
            .finish_non_exhaustive()
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CompleteMultipartWire<'a> {
    digest: &'a Sha256Digest,
    last_sequence: u64,
    size_bytes: u64,
}

struct WireRequest<'a> {
    method: S3ArtifactMethod,
    url: &'a str,
    operation_id: &'a str,
    body: &'a [u8],
    content_type: Option<&'static str>,
    checksum: Option<&'a str>,
    range: Option<&'a str>,
    max_response_bytes: usize,
}

impl WireRequest<'_> {
    fn context<'b>(
        &'b self,
        payload_sha256: &'b str,
        policy_headers: &'b S3ArtifactHeaders,
    ) -> S3ArtifactRequestContext<'b> {
        S3ArtifactRequestContext {
            method: self.method,
            url: self.url,
            operation_id: self.operation_id,
            payload_sha256,
            content_type: self.content_type,
            range: self.range,
            checksum: self.checksum,
            policy_headers,
        }
    }
}

struct WireResponse {
    status: u16,
    body: Vec<u8>,
    content_type: Option<String>,
    operation_id: Option<String>,
    checksum: Option<String>,
    content_range: Option<String>,
    policy_headers: S3ArtifactHeaders,
    headers: S3ArtifactResponseHeaders,
}

fn read_wire_response(
    response: ureq::http::Response<ureq::Body>,
    maximum: usize,
    policy_headers: S3ArtifactHeaders,
) -> Result<WireResponse, S3ArtifactError> {
    let status = response.status().as_u16();
    let content_type = response_header(&response, "content-type")?;
    let operation_id = response_header(&response, "x-winwincode-operation-id")?;
    let checksum = response_header(&response, "x-amz-meta-winwincode-sha256")?;
    let content_range = response_header(&response, "content-range")?;
    let headers = response_headers(&response)?;
    let mut reader = response.into_body().into_reader();
    let mut body = Vec::new();
    reader
        .by_ref()
        .take(
            u64::try_from(maximum)
                .map_err(|_| corrupt())?
                .saturating_add(1),
        )
        .read_to_end(&mut body)
        .map_err(|_| transport())?;
    if body.len() > maximum {
        return Err(corrupt());
    }
    Ok(WireResponse {
        status,
        body,
        content_type,
        operation_id,
        checksum,
        content_range,
        policy_headers,
        headers,
    })
}

fn response_headers(
    response: &ureq::http::Response<ureq::Body>,
) -> Result<S3ArtifactResponseHeaders, S3ArtifactError> {
    if response.headers().len() > MAX_HEADER_COUNT {
        return Err(corrupt());
    }
    let mut values = Vec::with_capacity(response.headers().len());
    for (name, value) in response.headers() {
        let value = value.to_str().map_err(|_| corrupt())?;
        if value.len() > MAX_HEADER_VALUE_BYTES || value.chars().any(char::is_control) {
            return Err(corrupt());
        }
        values.push((name.as_str().to_ascii_lowercase(), value.to_owned()));
    }
    Ok(S3ArtifactResponseHeaders(values))
}

fn response_header(
    response: &ureq::http::Response<ureq::Body>,
    name: &str,
) -> Result<Option<String>, S3ArtifactError> {
    response
        .headers()
        .get(name)
        .map(|value| {
            value
                .to_str()
                .ok()
                .filter(|value| {
                    value.len() <= MAX_HEADER_VALUE_BYTES && !value.chars().any(char::is_control)
                })
                .map(str::to_owned)
                .ok_or_else(corrupt)
        })
        .transpose()
}

fn require_response_status(
    response: &WireResponse,
    expected: u16,
    operation_id: &str,
) -> Result<(), S3ArtifactError> {
    if response.status != expected {
        return Err(status_error(response.status));
    }
    if response.operation_id.as_deref() != Some(operation_id) {
        return Err(corrupt());
    }
    Ok(())
}

fn require_empty_status(
    response: &WireResponse,
    expected: u16,
    operation_id: &str,
) -> Result<(), S3ArtifactError> {
    require_one_of_statuses(response, &[expected], operation_id)
}

fn require_one_of_statuses(
    response: &WireResponse,
    expected: &[u16],
    operation_id: &str,
) -> Result<(), S3ArtifactError> {
    if !expected.contains(&response.status) {
        return Err(status_error(response.status));
    }
    if response.operation_id.as_deref() != Some(operation_id) || !response.body.is_empty() {
        return Err(corrupt());
    }
    Ok(())
}

fn require_octet_stream(response: &WireResponse) -> Result<(), S3ArtifactError> {
    if !response
        .content_type
        .as_deref()
        .is_some_and(|value| value.eq_ignore_ascii_case("application/octet-stream"))
    {
        return Err(corrupt());
    }
    Ok(())
}

fn status_error(status: u16) -> S3ArtifactError {
    let kind = match status {
        400 | 422 => S3ArtifactErrorKind::Invalid,
        401 | 403 => S3ArtifactErrorKind::PermissionDenied,
        404 => S3ArtifactErrorKind::NotFound,
        409 => S3ArtifactErrorKind::Conflict,
        412 => S3ArtifactErrorKind::DigestMismatch,
        _ => S3ArtifactErrorKind::Transport,
    };
    S3ArtifactError::new(kind, "S3 Artifact request failed")
}

fn parse_content_range(value: &str, offset: u64, end: u64) -> Result<u64, S3ArtifactError> {
    let (bounds, total) = value
        .strip_prefix("bytes ")
        .and_then(|rest| rest.split_once('/'))
        .ok_or_else(corrupt)?;
    let (start, returned_end) = bounds.split_once('-').ok_or_else(corrupt)?;
    let start = start.parse::<u64>().map_err(|_| corrupt())?;
    let returned_end = returned_end.parse::<u64>().map_err(|_| corrupt())?;
    let total = total.parse::<u64>().map_err(|_| corrupt())?;
    if start != offset || returned_end != end || total <= end || total > MAX_SAFE_INTEGER {
        return Err(corrupt());
    }
    Ok(total)
}

fn upload_id(artifact_id: &ArtifactId) -> String {
    let value = stable_hash(
        b"winwincode.s3-artifact-upload.v2",
        &[artifact_id.0.as_bytes()],
    );
    format!("wwcu_{}", &value[..32])
}

fn operation_id(operation: &str, fields: &[&[u8]]) -> String {
    let mut all = Vec::with_capacity(fields.len() + 1);
    all.push(operation.as_bytes());
    all.extend_from_slice(fields);
    let value = stable_hash(b"winwincode.s3-artifact-operation.v2", &all);
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

fn digest_hex(digest: &Sha256Digest) -> Result<&str, S3ArtifactError> {
    digest
        .0
        .strip_prefix("sha256:")
        .filter(|value| {
            value.len() == 64
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
        })
        .ok_or_else(invalid)
}

fn validate_artifact_id(artifact_id: &ArtifactId) -> Result<(), S3ArtifactError> {
    let valid = artifact_id
        .0
        .strip_prefix("art_")
        .is_some_and(|suffix| suffix.len() == 26 && suffix.bytes().all(crockford_byte));
    if valid { Ok(()) } else { Err(invalid()) }
}

fn validate_base_url(value: &str) -> Result<(), S3ArtifactError> {
    let valid = value.len() <= MAX_URL_BYTES
        && value.starts_with("https://")
        && value.trim() == value
        && !value.ends_with('/')
        && !value.contains(['?', '#', '\\'])
        && !value.chars().any(char::is_control)
        && value
            .strip_prefix("https://")
            .and_then(|rest| rest.split_once('/'))
            .is_some_and(|(authority, path)| {
                !authority.is_empty()
                    && !authority.contains('@')
                    && !path.is_empty()
                    && path
                        .split('/')
                        .all(|segment| !segment.is_empty() && segment != "." && segment != "..")
            });
    if valid { Ok(()) } else { Err(invalid()) }
}

fn require_distinct_headers(
    first: &S3ArtifactHeaders,
    second: &S3ArtifactHeaders,
) -> Result<(), S3ArtifactError> {
    let mut names = BTreeSet::new();
    for (name, _) in first.iter().chain(second.iter()) {
        if !names.insert(name) {
            return Err(invalid());
        }
    }
    Ok(())
}

fn valid_header_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_HEADER_NAME_BYTES
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || matches!(
                    byte,
                    b'!' | b'#'
                        | b'$'
                        | b'%'
                        | b'&'
                        | b'\''
                        | b'*'
                        | b'+'
                        | b'-'
                        | b'.'
                        | b'^'
                        | b'_'
                        | b'`'
                        | b'|'
                        | b'~'
                )
        })
}

fn valid_header_value(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_HEADER_VALUE_BYTES
        && value.trim() == value
        && value.bytes().all(|byte| matches!(byte, 0x20..=0x7e))
}

fn reserved_header(value: &str) -> bool {
    matches!(
        value,
        "accept"
            | "connection"
            | "content-length"
            | "content-type"
            | "host"
            | "range"
            | "transfer-encoding"
            | "x-amz-meta-winwincode-sha256"
            | "x-winwincode-operation-id"
    )
}

fn crockford_byte(byte: u8) -> bool {
    byte.is_ascii_digit()
        || matches!(
            byte,
            b'A'..=b'H' | b'J' | b'K' | b'M' | b'N' | b'P'..=b'T' | b'V'..=b'Z'
        )
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

const fn invalid() -> S3ArtifactError {
    S3ArtifactError::new(S3ArtifactErrorKind::Invalid, "S3 Artifact input is invalid")
}

const fn corrupt() -> S3ArtifactError {
    S3ArtifactError::new(
        S3ArtifactErrorKind::CorruptResponse,
        "S3 Artifact response is corrupt",
    )
}

const fn transport() -> S3ArtifactError {
    S3ArtifactError::new(
        S3ArtifactErrorKind::Transport,
        "S3 Artifact transport failed",
    )
}
