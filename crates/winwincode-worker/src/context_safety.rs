// SPDX-License-Identifier: Apache-2.0

//! One Worker-owned policy for text that may enter bounded model context.

use std::{fmt, sync::OnceLock};

use sha2::{Digest, Sha256};
use winwincode_domain::Sha256Digest;
use winwincode_execution_port::debug_probe_delta_context::{
    ContextSafetyScanError, DebugContextSafetyScanner,
};
use winwincode_execution_port::generated::{
    DebugContextSafetyProfile, DebugContextSafetyScannerVersion,
};

/// Maximum UTF-8 bytes and Unicode scalar values accepted in one summary.
pub const MAX_SAFE_SUMMARY_CHARS: usize = 500;
/// Maximum UTF-8 bytes and Unicode scalar values accepted in one source snippet.
pub const MAX_SAFE_SNIPPET_CHARS: usize = 2_000;

const SECRET_SCAN_RULES: [&str; 7] = [
    "private-key:-----BEGIN (RSA |OPENSSH )?PRIVATE KEY-----",
    "bearer:Bearer [A-Za-z0-9._~+/=-]{12,}",
    "basic:Basic [A-Za-z0-9+/]{12,}={0,2}",
    "jwt:eyJ<base64url>.<base64url>.<base64url>",
    "provider:sk|github|aws|google|slack|npm token families",
    "url-userinfo:recognized scheme with authority credentials",
    "assignment:credential key [=:] secret value length >= 8",
];

const RAW_CONTEXT_MARKERS: [&str; 8] = [
    "raw log",
    "raw-log",
    "raw_log",
    "rawlog",
    "raw output",
    "raw-output",
    "raw_output",
    "rawoutput",
];

static CONTEXT_SAFETY_PROFILE: OnceLock<DebugContextSafetyProfile> = OnceLock::new();

/// Opaque text that passed the Worker context-safety policy.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct ValidatedSafeText(String);

impl ValidatedSafeText {
    /// Validates one model-facing summary.
    ///
    /// # Errors
    ///
    /// Rejects empty, multiline, oversized, credential-shaped, or raw-log text.
    pub fn try_summary(value: impl Into<String>) -> Result<Self, ContextSafetyError> {
        Self::try_new(value.into(), MAX_SAFE_SUMMARY_CHARS)
    }

    /// Validates one model-facing source snippet.
    ///
    /// # Errors
    ///
    /// Rejects empty, multiline, oversized, credential-shaped, or raw-log text.
    pub fn try_snippet(value: impl Into<String>) -> Result<Self, ContextSafetyError> {
        Self::try_new(value.into(), MAX_SAFE_SNIPPET_CHARS)
    }

    /// Borrows the validated text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Returns the validated owned text.
    #[must_use]
    pub fn into_inner(self) -> String {
        self.0
    }

    fn try_new(value: String, maximum_characters: usize) -> Result<Self, ContextSafetyError> {
        if value.trim().is_empty() {
            return Err(ContextSafetyError::Empty);
        }
        if value.chars().any(char::is_control) {
            return Err(ContextSafetyError::NotSingleLine);
        }
        if value.len() > maximum_characters || value.chars().count() > maximum_characters {
            return Err(ContextSafetyError::TooLong);
        }
        WorkerContextSafetyScanner.validate(&value)?;
        Ok(Self(value))
    }
}

impl fmt::Debug for ValidatedSafeText {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ValidatedSafeText(<validated>)")
    }
}

impl AsRef<str> for ValidatedSafeText {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

/// Stable reason that context text did not pass Worker validation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContextSafetyError {
    /// The value contains no visible content.
    Empty,
    /// The value is not one canonical line.
    NotSingleLine,
    /// The value exceeds the bound for its role.
    TooLong,
    /// The value contains credential-shaped material.
    SensitiveMaterial,
    /// The value is labelled as raw command or log output.
    RawContextMarker,
}

impl fmt::Display for ContextSafetyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Empty => "context text is empty",
            Self::NotSingleLine => "context text is not one line",
            Self::TooLong => "context text exceeds its limit",
            Self::SensitiveMaterial => "context text contains sensitive material",
            Self::RawContextMarker => "context text identifies raw output",
        })
    }
}

impl std::error::Error for ContextSafetyError {}

/// Worker scanner used by the D4 context sealing boundary.
#[derive(Clone, Copy, Debug, Default)]
pub struct WorkerContextSafetyScanner;

impl WorkerContextSafetyScanner {
    /// Applies the complete Worker context-safety policy to one value.
    ///
    /// # Errors
    ///
    /// Rejects credential-shaped or raw-log text without retaining the value.
    pub fn validate(&self, value: &str) -> Result<(), ContextSafetyError> {
        <Self as DebugContextSafetyScanner>::validate(self, value).map_err(|error| match error {
            ContextSafetyScanError::SensitiveMaterial => ContextSafetyError::SensitiveMaterial,
            ContextSafetyScanError::RawContextMaterial => ContextSafetyError::RawContextMarker,
        })
    }

    /// Returns the exact policy digest bound into D4 context receipts.
    #[must_use]
    pub fn profile(&self) -> &DebugContextSafetyProfile {
        CONTEXT_SAFETY_PROFILE.get_or_init(|| DebugContextSafetyProfile {
            scanner_policy_digest: context_safety_policy_digest(),
            scanner_version: DebugContextSafetyScannerVersion::WorkspaceSecretScanV1,
        })
    }
}

impl DebugContextSafetyScanner for WorkerContextSafetyScanner {
    fn profile(&self) -> &DebugContextSafetyProfile {
        Self::profile(self)
    }

    fn validate(&self, text: &str) -> Result<(), ContextSafetyScanError> {
        if contains_sensitive_material(text) {
            return Err(ContextSafetyScanError::SensitiveMaterial);
        }
        if contains_raw_context_marker(text) {
            return Err(ContextSafetyScanError::RawContextMaterial);
        }
        Ok(())
    }
}

pub(crate) fn contains_sensitive_material(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    contains_private_key(&lower)
        || contains_authorization_token(&lower, "bearer ", bearer_character)
        || contains_authorization_token(&lower, "basic ", basic_character)
        || contains_jwt(value)
        || contains_provider_token(value)
        || contains_url_userinfo(&lower)
        || contains_sensitive_assignment(&lower)
}

pub(crate) fn observation_secret_scan_version() -> String {
    let encoded = secret_scan_policy_digest_hex();
    format!("winwincode-secret-scan-v2-{}", &encoded[..16])
}

fn context_safety_policy_digest() -> Sha256Digest {
    let mut digest = Sha256::new();
    digest.update(b"winwincode.context-safety-rules.v1\0");
    update_rules_digest(&mut digest, &SECRET_SCAN_RULES);
    update_rules_digest(&mut digest, &RAW_CONTEXT_MARKERS);
    Sha256Digest(format!("sha256:{:x}", digest.finalize()))
}

fn secret_scan_policy_digest_hex() -> String {
    let mut digest = Sha256::new();
    digest.update(b"winwincode.observation-secret-scan-rules.v2\0");
    update_rules_digest(&mut digest, &SECRET_SCAN_RULES);
    format!("{:x}", digest.finalize())
}

fn update_rules_digest<const N: usize>(digest: &mut Sha256, rules: &[&str; N]) {
    for rule in rules {
        digest.update((rule.len() as u64).to_be_bytes());
        digest.update(rule.as_bytes());
    }
}

fn contains_raw_context_marker(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    RAW_CONTEXT_MARKERS
        .iter()
        .any(|marker| lower.contains(marker))
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

fn bearer_character(character: char) -> bool {
    character.is_ascii_alphanumeric() || "._~+/=-".contains(character)
}

fn basic_character(character: char) -> bool {
    character.is_ascii_alphanumeric() || "+/=".contains(character)
}

fn contains_authorization_token(value: &str, marker: &str, allowed: fn(char) -> bool) -> bool {
    value.match_indices(marker).any(|(index, _)| {
        let candidate = value[index + marker.len()..]
            .chars()
            .take_while(|character| allowed(*character))
            .collect::<String>();
        candidate.len() >= 12
            && !matches!(candidate.as_str(), "[redacted]" | "<redacted>" | "redacted")
    })
}

fn contains_jwt(value: &str) -> bool {
    value
        .split(|character: char| {
            character.is_ascii_whitespace() || "\"'()[]{}<>,;".contains(character)
        })
        .any(|token| {
            let mut segments = token.split('.');
            let Some(header) = segments.next() else {
                return false;
            };
            let Some(payload) = segments.next() else {
                return false;
            };
            let Some(signature) = segments.next() else {
                return false;
            };
            segments.next().is_none()
                && header.starts_with("eyJ")
                && [header, payload, signature].iter().all(|segment| {
                    !segment.is_empty()
                        && segment
                            .bytes()
                            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
                })
        })
}

fn contains_provider_token(value: &str) -> bool {
    [
        ("sk-", 16, TokenAlphabet::Mixed),
        ("ghp_", 20, TokenAlphabet::Mixed),
        ("gho_", 20, TokenAlphabet::Mixed),
        ("ghs_", 20, TokenAlphabet::Mixed),
        ("ghu_", 20, TokenAlphabet::Mixed),
        ("ghr_", 20, TokenAlphabet::Mixed),
        ("github_pat_", 20, TokenAlphabet::Mixed),
        ("AKIA", 16, TokenAlphabet::Upper),
        ("AIza", 35, TokenAlphabet::Mixed),
        ("xoxb-", 10, TokenAlphabet::Mixed),
        ("xoxa-", 10, TokenAlphabet::Mixed),
        ("xoxp-", 10, TokenAlphabet::Mixed),
        ("xoxr-", 10, TokenAlphabet::Mixed),
        ("xoxs-", 10, TokenAlphabet::Mixed),
        ("npm_", 20, TokenAlphabet::Alphanumeric),
    ]
    .iter()
    .any(|(prefix, minimum, alphabet)| {
        value.match_indices(prefix).any(|(index, _)| {
            (index == 0 || !value.as_bytes()[index - 1].is_ascii_alphanumeric())
                && value[index + prefix.len()..]
                    .bytes()
                    .take_while(|byte| alphabet.contains(*byte))
                    .count()
                    >= *minimum
        })
    })
}

#[derive(Clone, Copy)]
enum TokenAlphabet {
    Alphanumeric,
    Mixed,
    Upper,
}

impl TokenAlphabet {
    fn contains(self, byte: u8) -> bool {
        match self {
            Self::Alphanumeric => byte.is_ascii_alphanumeric(),
            Self::Mixed => byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'),
            Self::Upper => byte.is_ascii_uppercase() || byte.is_ascii_digit(),
        }
    }
}

fn contains_url_userinfo(value: &str) -> bool {
    let mut remainder = value;
    while let Some(scheme_end) = remainder.find("://") {
        let scheme = remainder[..scheme_end]
            .rsplit(|character: char| !character.is_ascii_alphabetic())
            .next()
            .unwrap_or("");
        let after_scheme = &remainder[scheme_end + 3..];
        let authority_end = after_scheme
            .find(|character: char| character.is_ascii_whitespace() || "/?#".contains(character))
            .unwrap_or(after_scheme.len());
        let authority = &after_scheme[..authority_end];
        if matches!(scheme, "http" | "https" | "ws" | "wss")
            && authority
                .rfind('@')
                .is_some_and(|at| authority[..at].contains(':'))
        {
            return true;
        }
        remainder = &after_scheme[authority_end..];
    }
    false
}

fn contains_sensitive_assignment(value: &str) -> bool {
    [
        "api-key",
        "api_key",
        "apikey",
        "authorization",
        "client-secret",
        "client_secret",
        "password",
        "passwd",
        "private-key",
        "private_key",
        "secret",
        "access-token",
        "access_token",
        "refresh-token",
        "refresh_token",
        "id-token",
        "id_token",
        "session-token",
        "session_token",
        "token",
    ]
    .iter()
    .any(|key| {
        value.match_indices(key).any(|(index, _)| {
            let boundary = index == 0 || !value.as_bytes()[index - 1].is_ascii_alphanumeric();
            let remainder = value[index + key.len()..].trim_start();
            let Some(remainder) = remainder.strip_prefix(['=', ':']) else {
                return false;
            };
            let candidate = remainder
                .trim_start_matches([' ', '\t', '\"', '\''])
                .chars()
                .take_while(|character| bearer_character(*character))
                .collect::<String>();
            boundary
                && candidate.len() >= 8
                && !matches!(candidate.as_str(), "[redacted]" | "<redacted>" | "redacted")
        })
    })
}
