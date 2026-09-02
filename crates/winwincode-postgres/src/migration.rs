// SPDX-License-Identifier: Apache-2.0

use sha2::{Digest, Sha256};
use winwincode_domain::Sha256Digest;

use crate::{PostgresError, PostgresErrorKind};

/// Current canonical `PostgreSQL` schema version.
pub const POSTGRES_SCHEMA_VERSION: u64 = 1;

const MIGRATION_V1_SQL: &str = r"SELECT pg_advisory_xact_lock(6305038718321181514);
CREATE TABLE winwincode_schema_migrations (
  version BIGINT PRIMARY KEY CHECK (version > 0),
  migration_digest TEXT NOT NULL CHECK (migration_digest ~ '^[0-9a-f]{64}$'),
  schema_digest TEXT NOT NULL CHECK (schema_digest ~ '^[0-9a-f]{64}$'),
  applied_at TIMESTAMPTZ NOT NULL
);
CREATE TABLE winwincode_product_state (
  scope_key BYTEA NOT NULL,
  stream_id TEXT NOT NULL CHECK (stream_id <> ''),
  revision BIGINT NOT NULL CHECK (revision > 0),
  payload BYTEA NOT NULL,
  payload_sha256 TEXT NOT NULL CHECK (payload_sha256 ~ '^[0-9a-f]{64}$'),
  PRIMARY KEY (scope_key, stream_id)
);
CREATE TABLE winwincode_command_receipts (
  scope_key BYTEA NOT NULL,
  actor_key BYTEA NOT NULL,
  request_id TEXT NOT NULL CHECK (request_id <> ''),
  command_digest TEXT NOT NULL CHECK (command_digest ~ '^[0-9a-f]{64}$'),
  stream_id TEXT NOT NULL CHECK (stream_id <> ''),
  revision BIGINT NOT NULL CHECK (revision > 0),
  PRIMARY KEY (scope_key, actor_key, request_id)
);
CREATE TABLE winwincode_outbox (
  sequence BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  scope_key BYTEA NOT NULL,
  actor_key BYTEA NOT NULL,
  request_id TEXT NOT NULL,
  event_id TEXT NOT NULL UNIQUE CHECK (event_id <> ''),
  topic TEXT NOT NULL CHECK (topic <> ''),
  payload BYTEA NOT NULL,
  projection_context BYTEA,
  published_at TIMESTAMPTZ,
  FOREIGN KEY (scope_key, actor_key, request_id)
    REFERENCES winwincode_command_receipts(scope_key, actor_key, request_id)
);
CREATE TABLE winwincode_aggregate_journals (
  scope_key BYTEA NOT NULL,
  aggregate_type TEXT NOT NULL,
  aggregate_id TEXT NOT NULL,
  manifest BYTEA NOT NULL,
  tail_sequence BIGINT NOT NULL CHECK (tail_sequence > 0),
  tail_digest TEXT NOT NULL CHECK (tail_digest ~ '^[0-9a-f]{64}$'),
  PRIMARY KEY (scope_key, aggregate_type, aggregate_id)
);
CREATE TABLE winwincode_aggregate_journal_records (
  scope_key BYTEA NOT NULL,
  aggregate_type TEXT NOT NULL,
  aggregate_id TEXT NOT NULL,
  sequence BIGINT NOT NULL CHECK (sequence > 0),
  digest TEXT NOT NULL CHECK (digest ~ '^[0-9a-f]{64}$'),
  payload BYTEA NOT NULL,
  PRIMARY KEY (scope_key, aggregate_type, aggregate_id, sequence),
  FOREIGN KEY (scope_key, aggregate_type, aggregate_id)
    REFERENCES winwincode_aggregate_journals(scope_key, aggregate_type, aggregate_id)
);
CREATE TABLE winwincode_audit_outbox (
  scope_key BYTEA NOT NULL,
  actor_key BYTEA NOT NULL,
  request_id TEXT NOT NULL,
  event_id TEXT NOT NULL UNIQUE,
  payload BYTEA NOT NULL,
  persisted_at TIMESTAMPTZ,
  PRIMARY KEY (scope_key, actor_key, request_id),
  FOREIGN KEY (scope_key, actor_key, request_id)
    REFERENCES winwincode_command_receipts(scope_key, actor_key, request_id)
);
CREATE INDEX winwincode_outbox_pending
  ON winwincode_outbox (sequence) WHERE published_at IS NULL;
CREATE INDEX winwincode_audit_outbox_pending
  ON winwincode_audit_outbox (event_id) WHERE persisted_at IS NULL;
ALTER TABLE winwincode_product_state ENABLE ROW LEVEL SECURITY;
ALTER TABLE winwincode_product_state FORCE ROW LEVEL SECURITY;
ALTER TABLE winwincode_command_receipts ENABLE ROW LEVEL SECURITY;
ALTER TABLE winwincode_command_receipts FORCE ROW LEVEL SECURITY;
ALTER TABLE winwincode_outbox ENABLE ROW LEVEL SECURITY;
ALTER TABLE winwincode_outbox FORCE ROW LEVEL SECURITY;
ALTER TABLE winwincode_aggregate_journals ENABLE ROW LEVEL SECURITY;
ALTER TABLE winwincode_aggregate_journals FORCE ROW LEVEL SECURITY;
ALTER TABLE winwincode_aggregate_journal_records ENABLE ROW LEVEL SECURITY;
ALTER TABLE winwincode_aggregate_journal_records FORCE ROW LEVEL SECURITY;
ALTER TABLE winwincode_audit_outbox ENABLE ROW LEVEL SECURITY;
ALTER TABLE winwincode_audit_outbox FORCE ROW LEVEL SECURITY;
CREATE POLICY winwincode_product_state_scope ON winwincode_product_state
  USING (scope_key = decode(current_setting('winwincode.scope_key', true), 'hex'))
  WITH CHECK (scope_key = decode(current_setting('winwincode.scope_key', true), 'hex'));
CREATE POLICY winwincode_command_receipts_scope ON winwincode_command_receipts
  USING (scope_key = decode(current_setting('winwincode.scope_key', true), 'hex'))
  WITH CHECK (scope_key = decode(current_setting('winwincode.scope_key', true), 'hex'));
CREATE POLICY winwincode_outbox_scope ON winwincode_outbox
  USING (scope_key = decode(current_setting('winwincode.scope_key', true), 'hex'))
  WITH CHECK (scope_key = decode(current_setting('winwincode.scope_key', true), 'hex'));
CREATE POLICY winwincode_aggregate_journals_scope ON winwincode_aggregate_journals
  USING (scope_key = decode(current_setting('winwincode.scope_key', true), 'hex'))
  WITH CHECK (scope_key = decode(current_setting('winwincode.scope_key', true), 'hex'));
CREATE POLICY winwincode_aggregate_journal_records_scope ON winwincode_aggregate_journal_records
  USING (scope_key = decode(current_setting('winwincode.scope_key', true), 'hex'))
  WITH CHECK (scope_key = decode(current_setting('winwincode.scope_key', true), 'hex'));
CREATE POLICY winwincode_audit_outbox_scope ON winwincode_audit_outbox
  USING (scope_key = decode(current_setting('winwincode.scope_key', true), 'hex'))
  WITH CHECK (scope_key = decode(current_setting('winwincode.scope_key', true), 'hex'));";

/// One immutable ordered `PostgreSQL` migration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PostgresMigration {
    version: u64,
    sql: &'static str,
    digest: Sha256Digest,
}

impl PostgresMigration {
    #[must_use]
    pub const fn version(&self) -> u64 {
        self.version
    }

    #[must_use]
    pub const fn sql(&self) -> &'static str {
        self.sql
    }

    #[must_use]
    pub const fn digest(&self) -> &Sha256Digest {
        &self.digest
    }
}

/// The sole canonical ordered `PostgreSQL` migration plan.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PostgresMigrationPlan {
    migrations: Vec<PostgresMigration>,
    digest: Sha256Digest,
}

impl PostgresMigrationPlan {
    /// Builds and validates the current immutable plan.
    ///
    /// # Errors
    ///
    /// Returns a migration conflict if the embedded versions or digests are
    /// no longer canonical.
    pub fn current() -> Result<Self, PostgresError> {
        let migration = PostgresMigration {
            version: POSTGRES_SCHEMA_VERSION,
            sql: MIGRATION_V1_SQL,
            digest: sha256(MIGRATION_V1_SQL.as_bytes()),
        };
        let digest = plan_digest(std::slice::from_ref(&migration));
        let plan = Self {
            migrations: vec![migration],
            digest,
        };
        plan.validate()?;
        Ok(plan)
    }

    #[must_use]
    pub fn migrations(&self) -> &[PostgresMigration] {
        &self.migrations
    }

    #[must_use]
    pub const fn digest(&self) -> &Sha256Digest {
        &self.digest
    }

    fn validate(&self) -> Result<(), PostgresError> {
        let versions_are_ordered = self
            .migrations
            .iter()
            .enumerate()
            .all(|(index, migration)| migration.version == index as u64 + 1);
        let digests_match = self
            .migrations
            .iter()
            .all(|migration| migration.digest == sha256(migration.sql.as_bytes()));
        if !versions_are_ordered
            || !digests_match
            || self.migrations.last().map(PostgresMigration::version)
                != Some(POSTGRES_SCHEMA_VERSION)
            || self.digest != plan_digest(&self.migrations)
        {
            return Err(PostgresError::new(PostgresErrorKind::MigrationConflict));
        }
        Ok(())
    }
}

/// Durable result of atomically applying the exact migration plan.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PostgresMigrationReceipt {
    version: u64,
    plan_digest: Sha256Digest,
    schema_digest: Sha256Digest,
}

impl PostgresMigrationReceipt {
    /// Builds the protocol receipt returned after commit.
    ///
    /// # Errors
    ///
    /// Rejects a noncanonical version or digest.
    pub fn try_new(
        version: u64,
        plan_digest: Sha256Digest,
        schema_digest: Sha256Digest,
    ) -> Result<Self, PostgresError> {
        if version != POSTGRES_SCHEMA_VERSION
            || !canonical_digest(&plan_digest)
            || !canonical_digest(&schema_digest)
        {
            return Err(PostgresError::new(PostgresErrorKind::CorruptData));
        }
        Ok(Self {
            version,
            plan_digest,
            schema_digest,
        })
    }

    #[must_use]
    pub const fn version(&self) -> u64 {
        self.version
    }

    #[must_use]
    pub const fn plan_digest(&self) -> &Sha256Digest {
        &self.plan_digest
    }

    #[must_use]
    pub const fn schema_digest(&self) -> &Sha256Digest {
        &self.schema_digest
    }
}

fn plan_digest(migrations: &[PostgresMigration]) -> Sha256Digest {
    let mut hasher = Sha256::new();
    hasher.update(b"winwincode.postgres.migration-plan.v1\0");
    for migration in migrations {
        hasher.update(migration.version.to_be_bytes());
        hasher.update(migration.digest.0.as_bytes());
    }
    lower_hex(hasher.finalize().as_slice())
}

pub(crate) fn sha256(bytes: &[u8]) -> Sha256Digest {
    lower_hex(Sha256::digest(bytes).as_slice())
}

pub(crate) fn canonical_digest(digest: &Sha256Digest) -> bool {
    digest.0.len() == 64
        && digest
            .0
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn lower_hex(bytes: &[u8]) -> Sha256Digest {
    let mut value = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(value, "{byte:02x}");
    }
    Sha256Digest(value)
}
