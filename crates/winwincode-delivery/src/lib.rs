// SPDX-License-Identifier: Apache-2.0

//! Canonical Delivery facts and their append-only persistence seam.

pub mod application;
pub mod domain;
pub mod projection;
pub mod store;

pub mod sqlite_workrun_migration;
pub mod workrun_migration;
pub use sqlite_workrun_migration::SqliteWorkRunMigration;
pub use workrun_migration::{
    WORKRUN_MIGRATION_SCHEMA_VERSION, WorkRunMigrationError, WorkRunMigrationOutcome,
};
