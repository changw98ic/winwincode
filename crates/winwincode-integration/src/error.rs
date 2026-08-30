// SPDX-License-Identifier: Apache-2.0

use std::fmt;

/// Stable integration failure categories.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IntegrationErrorKind {
    Invalid,
    TenantMismatch,
    NotFound,
    Conflict,
    CredentialRevoked,
    SignatureRejected,
    ConnectorRejected,
    Storage,
    CorruptState,
}

/// Secret-safe integration failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IntegrationError {
    kind: IntegrationErrorKind,
    message: &'static str,
}

impl IntegrationError {
    pub(crate) const fn new(kind: IntegrationErrorKind, message: &'static str) -> Self {
        Self { kind, message }
    }

    #[must_use]
    pub const fn kind(&self) -> IntegrationErrorKind {
        self.kind
    }
}

impl fmt::Display for IntegrationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message)
    }
}

impl std::error::Error for IntegrationError {}
