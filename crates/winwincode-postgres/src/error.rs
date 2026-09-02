// SPDX-License-Identifier: Apache-2.0

use std::fmt;

/// Closed `PostgreSQL` adapter failure categories.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PostgresErrorKind {
    InvalidInput,
    MigrationConflict,
    RevisionConflict,
    RequestConflict,
    RequestReplayMissing,
    JournalConflict,
    CorruptData,
    Unavailable,
    Closed,
}

/// Secret-safe failure that never carries a DSN or provider diagnostic.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PostgresError {
    kind: PostgresErrorKind,
    actual_revision: Option<u64>,
}

impl PostgresError {
    #[must_use]
    pub const fn new(kind: PostgresErrorKind) -> Self {
        Self {
            kind,
            actual_revision: None,
        }
    }

    #[must_use]
    pub const fn revision_conflict(actual_revision: u64) -> Self {
        Self {
            kind: PostgresErrorKind::RevisionConflict,
            actual_revision: Some(actual_revision),
        }
    }

    #[must_use]
    pub const fn kind(self) -> PostgresErrorKind {
        self.kind
    }

    #[must_use]
    pub const fn actual_revision(self) -> Option<u64> {
        self.actual_revision
    }
}

impl fmt::Display for PostgresError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self.kind {
            PostgresErrorKind::InvalidInput => "PostgreSQL adapter input is invalid",
            PostgresErrorKind::MigrationConflict => "PostgreSQL migration authority conflicts",
            PostgresErrorKind::RevisionConflict => "PostgreSQL state revision conflicts",
            PostgresErrorKind::RequestConflict => "PostgreSQL request identity conflicts",
            PostgresErrorKind::RequestReplayMissing => "PostgreSQL request replay is incomplete",
            PostgresErrorKind::JournalConflict => "PostgreSQL journal authority conflicts",
            PostgresErrorKind::CorruptData => "PostgreSQL durable data is invalid",
            PostgresErrorKind::Unavailable => "PostgreSQL storage is unavailable",
            PostgresErrorKind::Closed => "PostgreSQL storage is closed",
        })
    }
}

impl std::error::Error for PostgresError {}
