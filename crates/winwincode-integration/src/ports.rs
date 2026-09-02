// SPDX-License-Identifier: Apache-2.0

use std::fmt;

use crate::{
    ConnectorAuthority, InboundNormalizationContext, NormalizedInboundEvent, OutboundCallReceipt,
    OutboundClaim,
};

/// Secret-safe signature verification failure category.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SignatureVerificationErrorKind {
    Rejected,
    CredentialRevoked,
}

/// Signature verification failure without signature, payload, or secret diagnostics.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SignatureVerificationError {
    kind: SignatureVerificationErrorKind,
}

impl SignatureVerificationError {
    #[must_use]
    pub const fn rejected() -> Self {
        Self {
            kind: SignatureVerificationErrorKind::Rejected,
        }
    }

    #[must_use]
    pub const fn credential_revoked() -> Self {
        Self {
            kind: SignatureVerificationErrorKind::CredentialRevoked,
        }
    }

    #[must_use]
    pub const fn kind(self) -> SignatureVerificationErrorKind {
        self.kind
    }
}

impl fmt::Display for SignatureVerificationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("webhook signature was rejected")
    }
}

impl std::error::Error for SignatureVerificationError {}

/// Credential-aware verifier. Implementations resolve only the supplied
/// credential reference and never return secret material.
pub trait WebhookSignatureVerifier {
    /// Verifies one exact raw request against the connector authority.
    ///
    /// # Errors
    ///
    /// Returns a secret-safe failure when the credential is revoked/missing or
    /// the signature does not authenticate the exact payload.
    fn verify(
        &mut self,
        authority: &ConnectorAuthority,
        signature: &[u8],
        payload: &[u8],
    ) -> Result<(), SignatureVerificationError>;
}

/// Stable connector call failure category.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConnectorCallErrorKind {
    Retryable,
    Permanent,
    CredentialRevoked,
}

/// Secret-safe protocol adapter failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConnectorCallError {
    kind: ConnectorCallErrorKind,
    code: String,
    retry_after_millis: Option<u64>,
}

impl ConnectorCallError {
    /// Builds a stable adapter error without remote body or credentials.
    ///
    /// # Errors
    ///
    /// Rejects an empty, overlong, or non-portable code.
    pub fn try_new(
        kind: ConnectorCallErrorKind,
        code: impl Into<String>,
    ) -> Result<Self, crate::IntegrationError> {
        let code = code.into();
        let valid = !code.is_empty()
            && code.len() <= 64
            && code
                .bytes()
                .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_');
        if !valid {
            return Err(crate::model::invalid());
        }
        Ok(Self {
            kind,
            code,
            retry_after_millis: None,
        })
    }

    /// Builds a retryable error carrying a provider lower bound for the next attempt.
    ///
    /// # Errors
    ///
    /// Rejects an invalid code, zero delay, or a delay outside the portable time range.
    pub fn retryable_after(
        code: impl Into<String>,
        retry_after_millis: u64,
    ) -> Result<Self, crate::IntegrationError> {
        if retry_after_millis == 0 || retry_after_millis > crate::model::MAX_SAFE_INTEGER {
            return Err(crate::model::invalid());
        }
        let mut error = Self::try_new(ConnectorCallErrorKind::Retryable, code)?;
        error.retry_after_millis = Some(retry_after_millis);
        Ok(error)
    }

    #[must_use]
    pub const fn kind(&self) -> ConnectorCallErrorKind {
        self.kind
    }

    #[must_use]
    pub fn code(&self) -> &str {
        &self.code
    }

    #[must_use]
    pub const fn retry_after_millis(&self) -> Option<u64> {
        self.retry_after_millis
    }
}

impl fmt::Display for ConnectorCallError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("connector call failed")
    }
}

impl std::error::Error for ConnectorCallError {}

/// Provider-specific protocol mapping only. Business commands remain in a
/// separate formal Control Plane adapter consuming durable inbound dispatches.
pub trait ConnectorPort {
    /// Normalizes one authenticated raw payload into a canonical command fact.
    ///
    /// # Errors
    ///
    /// Returns a stable adapter failure for unsupported or invalid payloads.
    fn normalize_inbound(
        &mut self,
        authority: &ConnectorAuthority,
        context: &InboundNormalizationContext,
        payload: &[u8],
    ) -> Result<NormalizedInboundEvent, ConnectorCallError>;

    /// Performs one retry-stable remote operation. The claim's operation key
    /// is the provider idempotency key for every retry and lease recovery.
    ///
    /// # Errors
    ///
    /// Returns a stable retryable, permanent, or revoked-credential outcome.
    fn deliver_outbound(
        &mut self,
        claim: &OutboundClaim,
    ) -> Result<OutboundCallReceipt, ConnectorCallError>;
}
