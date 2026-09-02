// SPDX-License-Identifier: Apache-2.0

use std::fmt;

/// Stable backup/restore failure categories.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BackupErrorKind {
    Invalid,
    UnsupportedVersion,
    Integrity,
    TenantMismatch,
    Incomplete,
    Conflict,
    Unavailable,
    Governance,
}

/// Adapter-neutral error without backend diagnostics or secret values.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackupError {
    kind: BackupErrorKind,
}

impl BackupError {
    #[must_use]
    pub const fn kind(&self) -> BackupErrorKind {
        self.kind
    }

    pub(crate) const fn new(kind: BackupErrorKind) -> Self {
        Self { kind }
    }

    pub(crate) const fn invalid() -> Self {
        Self::new(BackupErrorKind::Invalid)
    }

    pub(crate) const fn integrity() -> Self {
        Self::new(BackupErrorKind::Integrity)
    }

    pub(crate) const fn tenant() -> Self {
        Self::new(BackupErrorKind::TenantMismatch)
    }

    pub(crate) const fn incomplete() -> Self {
        Self::new(BackupErrorKind::Incomplete)
    }

    pub(crate) const fn conflict() -> Self {
        Self::new(BackupErrorKind::Conflict)
    }

    pub(crate) const fn unavailable() -> Self {
        Self::new(BackupErrorKind::Unavailable)
    }

    pub(crate) const fn governance() -> Self {
        Self::new(BackupErrorKind::Governance)
    }
}

impl fmt::Display for BackupError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self.kind {
            BackupErrorKind::Invalid => "backup facts are invalid",
            BackupErrorKind::UnsupportedVersion => "backup format is unsupported",
            BackupErrorKind::Integrity => "backup integrity verification failed",
            BackupErrorKind::TenantMismatch => "backup tenant scope does not match",
            BackupErrorKind::Incomplete => "backup is incomplete",
            BackupErrorKind::Conflict => "backup operation conflicts with durable state",
            BackupErrorKind::Unavailable => "backup storage is unavailable",
            BackupErrorKind::Governance => "backup governance evaluation failed",
        })
    }
}

impl std::error::Error for BackupError {}
