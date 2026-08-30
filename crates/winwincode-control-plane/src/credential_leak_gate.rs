// SPDX-License-Identifier: Apache-2.0

//! Fail-closed Credential output inspection.
//!
//! The gate complements typed, secret-free DTOs. It fingerprints resolved
//! secrets without retaining their bytes, applies an explicit JSON field
//! policy, and rejects high-confidence credential encodings. It never attempts
//! to repair output with string replacement.

use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use winwincode_api::generated::StrongFlowReadCursor;

use crate::credential_reference::ResolvedSecret;

const MAX_NESTING_DEPTH: usize = 64;
const MAX_DURABLE_FINGERPRINT_BYTES: usize = 64 * 1024;
const DURABLE_FINGERPRINT_SCHEMA: &str = "winwincode.credential-fingerprint.v1";

/// Output seams that must never receive Credential material.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum CredentialOutputBoundary {
    Log,
    Error,
    Debug,
    Serialization,
    Persistence,
    Event,
    Audit,
    Artifact,
    Evidence,
    Http,
    WebSocket,
    ReleasePackage,
}

impl CredentialOutputBoundary {
    const fn label(self) -> &'static str {
        match self {
            Self::Log => "log",
            Self::Error => "error",
            Self::Debug => "Debug",
            Self::Serialization => "serialization",
            Self::Persistence => "persistence",
            Self::Event => "event",
            Self::Audit => "audit",
            Self::Artifact => "Artifact",
            Self::Evidence => "Evidence",
            Self::Http => "HTTP",
            Self::WebSocket => "WebSocket",
            Self::ReleasePackage => "release package",
        }
    }
}

/// Stable diagnostic category. It identifies the failed policy, never the
/// matched value, field contents, or provider response.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CredentialLeakErrorKind {
    ExactSecret,
    ForbiddenField,
    RecognizedEncoding,
    InvalidOutput,
}

/// Secret-free rejection from [`CredentialLeakGate`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CredentialLeakError {
    boundary: CredentialOutputBoundary,
    kind: CredentialLeakErrorKind,
}

impl CredentialLeakError {
    #[must_use]
    pub const fn boundary(&self) -> CredentialOutputBoundary {
        self.boundary
    }

    #[must_use]
    pub const fn kind(&self) -> CredentialLeakErrorKind {
        self.kind
    }
}

impl fmt::Display for CredentialLeakError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "Credential output rejected at the {} boundary ({})",
            self.boundary.label(),
            match self.kind {
                CredentialLeakErrorKind::ExactSecret => "exact secret fingerprint",
                CredentialLeakErrorKind::ForbiddenField => "forbidden field policy",
                CredentialLeakErrorKind::RecognizedEncoding => "recognized credential encoding",
                CredentialLeakErrorKind::InvalidOutput => "invalid output shape",
            }
        )
    }
}

impl std::error::Error for CredentialLeakError {}

#[derive(Clone, Eq, PartialEq)]
struct SecretFingerprint {
    byte_length: usize,
    sha256: [u8; 32],
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DurableSecretFingerprints {
    schema: String,
    fingerprints: Vec<DurableSecretFingerprint>,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DurableSecretFingerprint {
    byte_length: usize,
    sha256: String,
}

/// Output gate holding only length/digest fingerprints of secrets it has seen.
///
/// Debug and serialization are intentionally absent so the fingerprint set
/// cannot itself become a public or persisted contract.
#[derive(Default)]
pub struct CredentialLeakGate {
    fingerprints: Vec<SecretFingerprint>,
}

impl CredentialLeakGate {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            fingerprints: Vec::new(),
        }
    }

    /// Copies only one-way fingerprints for another in-process output seam.
    /// Secret bytes remain owned by the canonical `ResolvedSecret` value.
    pub(crate) fn fingerprint_snapshot(&self) -> Self {
        Self {
            fingerprints: self.fingerprints.clone(),
        }
    }

    pub(crate) fn to_durable_fingerprint_json(&self) -> Result<Vec<u8>, CredentialLeakError> {
        let durable = DurableSecretFingerprints {
            schema: DURABLE_FINGERPRINT_SCHEMA.to_owned(),
            fingerprints: self
                .fingerprints
                .iter()
                .map(|fingerprint| DurableSecretFingerprint {
                    byte_length: fingerprint.byte_length,
                    sha256: lower_hex(&fingerprint.sha256),
                })
                .collect(),
        };
        serde_json::to_vec(&durable).map_err(|_| invalid_persisted_fingerprint())
    }

    pub(crate) fn from_durable_fingerprint_json(bytes: &[u8]) -> Result<Self, CredentialLeakError> {
        if bytes.is_empty() || bytes.len() > MAX_DURABLE_FINGERPRINT_BYTES {
            return Err(invalid_persisted_fingerprint());
        }
        let durable: DurableSecretFingerprints =
            serde_json::from_slice(bytes).map_err(|_| invalid_persisted_fingerprint())?;
        if durable.schema != DURABLE_FINGERPRINT_SCHEMA
            || serde_json::to_vec(&durable).map_err(|_| invalid_persisted_fingerprint())? != bytes
        {
            return Err(invalid_persisted_fingerprint());
        }
        let mut fingerprints = durable
            .fingerprints
            .into_iter()
            .map(|fingerprint| {
                if fingerprint.byte_length == 0 {
                    return Err(invalid_persisted_fingerprint());
                }
                Ok(SecretFingerprint {
                    byte_length: fingerprint.byte_length,
                    sha256: parse_lower_hex(&fingerprint.sha256)?,
                })
            })
            .collect::<Result<Vec<_>, CredentialLeakError>>()?;
        let original = fingerprints.clone();
        fingerprints.sort_by(|left, right| {
            left.byte_length
                .cmp(&right.byte_length)
                .then(left.sha256.cmp(&right.sha256))
        });
        fingerprints.dedup();
        if fingerprints != original {
            return Err(invalid_persisted_fingerprint());
        }
        Ok(Self { fingerprints })
    }

    /// Adds one resolved secret fingerprint without retaining or cloning its
    /// bytes. Resolution remains owned by the canonical `SecretStorePort`.
    pub fn track_secret(&mut self, secret: &ResolvedSecret) {
        let fingerprint = SecretFingerprint {
            byte_length: secret.expose().len(),
            sha256: Sha256::digest(secret.expose()).into(),
        };
        if !self.fingerprints.contains(&fingerprint) {
            self.fingerprints.push(fingerprint);
            self.fingerprints.sort_by(|left, right| {
                left.byte_length
                    .cmp(&right.byte_length)
                    .then(left.sha256.cmp(&right.sha256))
            });
        }
    }

    /// Inspects arbitrary log, error, Artifact, Evidence, or package bytes.
    ///
    /// # Errors
    ///
    /// Rejects an exact tracked secret or a high-confidence Credential syntax.
    pub fn inspect_bytes(
        &self,
        boundary: CredentialOutputBoundary,
        bytes: &[u8],
    ) -> Result<(), CredentialLeakError> {
        for fingerprint in &self.fingerprints {
            if fingerprint.byte_length <= bytes.len()
                && bytes
                    .windows(fingerprint.byte_length)
                    .any(|window| <[u8; 32]>::from(Sha256::digest(window)) == fingerprint.sha256)
            {
                return Err(CredentialLeakError {
                    boundary,
                    kind: CredentialLeakErrorKind::ExactSecret,
                });
            }
        }
        if std::str::from_utf8(bytes).is_ok_and(contains_recognized_credential) {
            return Err(CredentialLeakError {
                boundary,
                kind: CredentialLeakErrorKind::RecognizedEncoding,
            });
        }
        Ok(())
    }

    /// Inspects a typed serializable output before it crosses a public or
    /// durable seam.
    ///
    /// # Errors
    ///
    /// Fails closed on serialization, excessive nesting, forbidden fields, or
    /// Credential material.
    pub fn inspect_serializable<T: Serialize + ?Sized>(
        &self,
        boundary: CredentialOutputBoundary,
        value: &T,
    ) -> Result<(), CredentialLeakError> {
        let value = serde_json::to_value(value).map_err(|_| CredentialLeakError {
            boundary,
            kind: CredentialLeakErrorKind::InvalidOutput,
        })?;
        self.inspect_value(boundary, &value, "", 0, false)
    }

    /// Inspects canonical JSON bytes using both exact-byte and field-aware
    /// checks.
    ///
    /// # Errors
    ///
    /// Fails closed if the bytes are not valid JSON.
    pub fn inspect_json_bytes(
        &self,
        boundary: CredentialOutputBoundary,
        bytes: &[u8],
    ) -> Result<(), CredentialLeakError> {
        self.inspect_bytes(boundary, bytes)?;
        let value: Value = serde_json::from_slice(bytes).map_err(|_| CredentialLeakError {
            boundary,
            kind: CredentialLeakErrorKind::InvalidOutput,
        })?;
        self.inspect_value(boundary, &value, "", 0, false)
    }

    fn inspect_value(
        &self,
        boundary: CredentialOutputBoundary,
        value: &Value,
        key: &str,
        depth: usize,
        canonical_cursor_token: bool,
    ) -> Result<(), CredentialLeakError> {
        if depth > MAX_NESTING_DEPTH {
            return Err(CredentialLeakError {
                boundary,
                kind: CredentialLeakErrorKind::InvalidOutput,
            });
        }
        if forbidden_field(key, value) && !(canonical_cursor_token && normalize_key(key) == "token")
        {
            return Err(CredentialLeakError {
                boundary,
                kind: CredentialLeakErrorKind::ForbiddenField,
            });
        }
        match value {
            Value::String(text) => self.inspect_bytes(boundary, text.as_bytes()),
            Value::Array(values) => {
                for value in values {
                    self.inspect_value(boundary, value, "", depth + 1, false)?;
                }
                Ok(())
            }
            Value::Object(values) => {
                let canonical_cursor = canonical_strongflow_cursor(value);
                for (child_key, child) in values {
                    self.inspect_bytes(boundary, child_key.as_bytes())?;
                    self.inspect_value(
                        boundary,
                        child,
                        child_key,
                        depth + 1,
                        canonical_cursor && child_key == "token",
                    )?;
                }
                Ok(())
            }
            Value::Null | Value::Bool(_) | Value::Number(_) => Ok(()),
        }
    }
}

fn canonical_strongflow_cursor(value: &Value) -> bool {
    let Ok(cursor) = serde_json::from_value::<StrongFlowReadCursor>(value.clone()) else {
        return false;
    };
    cursor.token.len() == 69
        && cursor.token.strip_prefix("sfc1_").is_some_and(|digest| {
            digest.len() == 64
                && digest
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        })
}

fn invalid_persisted_fingerprint() -> CredentialLeakError {
    CredentialLeakError {
        boundary: CredentialOutputBoundary::Persistence,
        kind: CredentialLeakErrorKind::InvalidOutput,
    }
}

fn lower_hex(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(char::from(ALPHABET[usize::from(byte >> 4)]));
        output.push(char::from(ALPHABET[usize::from(byte & 0x0f)]));
    }
    output
}

fn parse_lower_hex(value: &str) -> Result<[u8; 32], CredentialLeakError> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(invalid_persisted_fingerprint());
    }
    let mut digest = [0_u8; 32];
    for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
        digest[index] = (hex_nibble(pair[0])? << 4) | hex_nibble(pair[1])?;
    }
    Ok(digest)
}

fn hex_nibble(byte: u8) -> Result<u8, CredentialLeakError> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        _ => Err(invalid_persisted_fingerprint()),
    }
}

fn forbidden_field(key: &str, value: &Value) -> bool {
    if key.is_empty() {
        return false;
    }
    let normalized = normalize_key(key);
    if matches!(
        normalized.as_str(),
        "credentialreferenceid" | "credentialreferenceids" | "credentialref"
    ) {
        return false;
    }
    if normalized == "secretstate" {
        return !matches!(
            value,
            Value::String(state)
                if matches!(
                    state.as_str(),
                    "available" | "revoked" | "missing" | "unavailable"
                )
        );
    }
    matches!(
        normalized.as_str(),
        "apikey"
            | "authorization"
            | "credential"
            | "credentials"
            | "password"
            | "passwd"
            | "privatekey"
            | "secret"
            | "clientsecret"
            | "token"
            | "accesstoken"
            | "refreshtoken"
            | "idtoken"
            | "sessiontoken"
            | "vaultlocator"
            | "credentiallocator"
            | "providercredential"
            | "secretmaterial"
    ) && !safe_placeholder(value)
}

fn normalize_key(key: &str) -> String {
    key.chars()
        .filter(char::is_ascii_alphanumeric)
        .flat_map(char::to_lowercase)
        .collect()
}

fn safe_placeholder(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::String(value) => matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "[redacted]"
                | "<redacted>"
                | "redacted"
                | "credential-reference"
                | "reference-only"
                | "dsh-reference-only"
        ),
        _ => false,
    }
}

fn contains_recognized_credential(value: &str) -> bool {
    let normalized = value.to_ascii_lowercase();
    contains_private_key(&normalized)
        || contains_bearer(&normalized)
        || contains_url_userinfo(&normalized)
        || contains_provider_token(&normalized)
        || contains_sensitive_assignment(&normalized)
}

fn contains_private_key(value: &str) -> bool {
    [
        "-----begin private key-----",
        "-----begin rsa private key-----",
        "-----begin openssh private key-----",
    ]
    .iter()
    .any(|marker| value.contains(marker))
}

fn contains_bearer(value: &str) -> bool {
    let mut remainder = value;
    while let Some(index) = remainder.find("bearer ") {
        let candidate = remainder[index + "bearer ".len()..]
            .split(|character: char| {
                character.is_ascii_whitespace() || ",;\"'}]".contains(character)
            })
            .next()
            .unwrap_or("");
        if !candidate.is_empty() && candidate != "[redacted" && candidate != "<redacted>" {
            return true;
        }
        remainder = &remainder[index + "bearer ".len()..];
    }
    false
}

fn contains_provider_token(value: &str) -> bool {
    [
        ("sk-", 16),
        ("ghp_", 20),
        ("gho_", 20),
        ("ghs_", 20),
        ("ghu_", 20),
        ("github_pat_", 20),
        ("xoxb-", 10),
        ("xoxp-", 10),
        ("npm_", 20),
    ]
    .iter()
    .any(|(prefix, minimum)| {
        value.match_indices(prefix).any(|(index, _)| {
            // A provider token prefix must begin at a value boundary.  Without
            // this check, ordinary public identifiers such as
            // `delivery-task-breakdown-transaction` contain the `sk-` suffix of
            // `task-` and are incorrectly rejected by the public WSS gate.
            // Keep scanning every string and preserve fail-closed rejection for
            // an actual token after a delimiter or at the start of a value.
            (index == 0 || !value.as_bytes()[index - 1].is_ascii_alphanumeric())
                && value[index + prefix.len()..]
                    .bytes()
                    .take_while(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
                    .count()
                    >= *minimum
        })
    })
}

fn contains_sensitive_assignment(value: &str) -> bool {
    [
        "api_key",
        "apikey",
        "authorization",
        "client_secret",
        "password",
        "private_key",
        "secret",
        "token",
    ]
    .iter()
    .any(|key| {
        ['=', ':'].iter().any(|separator| {
            let pattern = format!("{key}{separator}");
            value.match_indices(&pattern).any(|(index, _)| {
                let candidate = value[index + pattern.len()..]
                    .trim_start_matches([' ', '\t', '\"', '\''])
                    .split(|character: char| {
                        character.is_ascii_whitespace() || ",;\"'}]".contains(character)
                    })
                    .next()
                    .unwrap_or("");
                !candidate.is_empty()
                    && !matches!(candidate, "[redacted" | "<redacted>" | "redacted")
            })
        })
    })
}

fn contains_url_userinfo(value: &str) -> bool {
    let mut remainder = value;
    while let Some(scheme_end) = remainder.find("://") {
        let after_scheme = &remainder[scheme_end + 3..];
        let authority_end = after_scheme
            .find(|character: char| character.is_ascii_whitespace() || "/?#".contains(character))
            .unwrap_or(after_scheme.len());
        let authority = &after_scheme[..authority_end];
        if authority
            .rfind('@')
            .is_some_and(|at| authority[..at].contains(':'))
        {
            return true;
        }
        remainder = &after_scheme[authority_end..];
    }
    false
}
