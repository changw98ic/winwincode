// SPDX-License-Identifier: Apache-2.0

use std::fmt;

/// Stable drill failure categories.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DrillErrorKind {
    Invalid,
    Integrity,
    TenantMismatch,
    DrainIncomplete,
    Conflict,
    Unavailable,
}

/// Adapter-neutral drill failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DrillError {
    kind: DrillErrorKind,
}

impl DrillError {
    #[must_use]
    pub const fn kind(self) -> DrillErrorKind {
        self.kind
    }

    pub(crate) const fn invalid() -> Self {
        Self {
            kind: DrillErrorKind::Invalid,
        }
    }

    pub(crate) const fn integrity() -> Self {
        Self {
            kind: DrillErrorKind::Integrity,
        }
    }

    pub(crate) const fn tenant() -> Self {
        Self {
            kind: DrillErrorKind::TenantMismatch,
        }
    }

    pub(crate) const fn drain() -> Self {
        Self {
            kind: DrillErrorKind::DrainIncomplete,
        }
    }

    pub(crate) const fn conflict() -> Self {
        Self {
            kind: DrillErrorKind::Conflict,
        }
    }

    pub(crate) const fn unavailable() -> Self {
        Self {
            kind: DrillErrorKind::Unavailable,
        }
    }
}

impl fmt::Display for DrillError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self.kind {
            DrillErrorKind::Invalid => "drill facts are invalid",
            DrillErrorKind::Integrity => "drill integrity verification failed",
            DrillErrorKind::TenantMismatch => "drill tenant scope does not match",
            DrillErrorKind::DrainIncomplete => "control plane drain is incomplete",
            DrillErrorKind::Conflict => "drill conflicts with durable state",
            DrillErrorKind::Unavailable => "drill dependency is unavailable",
        })
    }
}

impl std::error::Error for DrillError {}
