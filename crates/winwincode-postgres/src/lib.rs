// SPDX-License-Identifier: Apache-2.0

//! `PostgreSQL` migration and transaction adapter for canonical Control Plane storage.

mod backup;
mod error;
mod migration;
mod protocol;
mod storage;

pub use backup::PostgresBackupSnapshotSource;
pub use error::{PostgresError, PostgresErrorKind};
pub use migration::{
    POSTGRES_SCHEMA_VERSION, PostgresMigration, PostgresMigrationPlan, PostgresMigrationReceipt,
};
pub use protocol::{
    PostgresCommitPlan, PostgresProtocolPort, PostgresSnapshotExport, PostgresTransactionStage,
};
pub use storage::PostgresStorage;
