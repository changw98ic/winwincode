// SPDX-License-Identifier: Apache-2.0

//! Durable enterprise quota policy and reservation ledger.
//!
//! This ledger accounts only for enterprise allowance. Provider request pools
//! and scheduler slots remain the operational capacity authorities.

use std::collections::HashSet;
use std::fmt;

use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use winwincode_domain::{
    ArtifactId, DeliveryId, ExecutionJobId, Instant, ModelExchangeId, OrganizationId,
    ProductSessionId, ProjectId, PublicationId, RepositoryId, RequestId, UserId, WorkspaceId,
};

use crate::{
    EnterpriseUsageAttribution, EnterpriseUsageEntry, EnterpriseUsageError,
    EnterpriseUsageErrorKind, EnterpriseUsageFilter, EnterpriseUsageSource, EnterpriseUsageTotals,
    SqliteStorage, StorageError,
};

const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
const MAX_ACTIVE_RESERVATIONS_PER_BOUNDARY: u64 = 100_000;
const CROCKFORD_BASE32: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
const QUOTA_SCHEMA: &str = r"
CREATE TABLE IF NOT EXISTS enterprise_quota_policies (
    boundary_key TEXT PRIMARY KEY NOT NULL,
    boundary_json TEXT NOT NULL,
    revision INTEGER NOT NULL CHECK (revision > 0),
    limits_json TEXT NOT NULL,
    policy_digest TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS enterprise_quota_reservations (
    reservation_id TEXT PRIMARY KEY NOT NULL,
    request_digest TEXT NOT NULL,
    record_json TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('active', 'released', 'settled')),
    revision INTEGER NOT NULL CHECK (revision > 0),
    reserved_tokens INTEGER NOT NULL CHECK (reserved_tokens >= 0),
    reserved_provider_cost_micros INTEGER NOT NULL CHECK (reserved_provider_cost_micros >= 0),
    reserved_worker_cost_microunits INTEGER NOT NULL CHECK (reserved_worker_cost_microunits >= 0),
    reserved_worker_runtime_millis INTEGER NOT NULL CHECK (reserved_worker_runtime_millis >= 0),
    reserved_storage_bytes INTEGER NOT NULL CHECK (reserved_storage_bytes >= 0),
    reserved_operations INTEGER NOT NULL CHECK (reserved_operations >= 0),
    source_seal_key TEXT UNIQUE NOT NULL,
    settled_usage_sequence INTEGER UNIQUE
        REFERENCES enterprise_usage_entries(sequence) ON DELETE RESTRICT
);
CREATE TABLE IF NOT EXISTS enterprise_quota_reservation_boundaries (
    reservation_id TEXT NOT NULL,
    boundary_key TEXT NOT NULL,
    ordinal INTEGER NOT NULL CHECK (ordinal >= 0),
    PRIMARY KEY (reservation_id, boundary_key),
    UNIQUE (reservation_id, ordinal),
    FOREIGN KEY (reservation_id) REFERENCES enterprise_quota_reservations(reservation_id)
        ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS enterprise_quota_active_by_boundary
    ON enterprise_quota_reservation_boundaries (boundary_key, reservation_id);
CREATE TABLE IF NOT EXISTS enterprise_quota_terminal_receipts (
    request_id TEXT PRIMARY KEY NOT NULL,
    reservation_id TEXT UNIQUE NOT NULL,
    command_kind TEXT NOT NULL CHECK (command_kind IN ('release', 'settle')),
    command_digest TEXT NOT NULL,
    terminal_revision INTEGER NOT NULL CHECK (terminal_revision > 0),
    record_digest TEXT NOT NULL,
    FOREIGN KEY (reservation_id) REFERENCES enterprise_quota_reservations(reservation_id)
        ON DELETE RESTRICT
);
";

/// One independently configurable enterprise quota boundary.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind", deny_unknown_fields)]
pub enum EnterpriseQuotaBoundary {
    Organization {
        organization_id: OrganizationId,
    },
    Workspace {
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
    },
    Project {
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
    },
    Repository {
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        repository_id: RepositoryId,
    },
    Delivery {
        organization_id: OrganizationId,
        delivery_id: DeliveryId,
    },
    ProductSession {
        organization_id: OrganizationId,
        product_session_id: ProductSessionId,
    },
    User {
        organization_id: OrganizationId,
        user_id: UserId,
    },
}

/// Quantities reserved by one enterprise operation.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EnterpriseQuotaAmounts {
    pub tokens: u64,
    pub provider_cost_micros: u64,
    pub worker_cost_microunits: u64,
    pub worker_runtime_millis: u64,
    pub storage_bytes: u64,
    pub operations: u64,
}

impl EnterpriseQuotaAmounts {
    fn checked_add(self, other: Self) -> Result<Self, EnterpriseQuotaError> {
        Ok(Self {
            tokens: checked_add(self.tokens, other.tokens, "tokens")?,
            provider_cost_micros: checked_add(
                self.provider_cost_micros,
                other.provider_cost_micros,
                "providerCostMicros",
            )?,
            worker_cost_microunits: checked_add(
                self.worker_cost_microunits,
                other.worker_cost_microunits,
                "workerCostMicrounits",
            )?,
            worker_runtime_millis: checked_add(
                self.worker_runtime_millis,
                other.worker_runtime_millis,
                "workerRuntimeMillis",
            )?,
            storage_bytes: checked_add(self.storage_bytes, other.storage_bytes, "storageBytes")?,
            operations: checked_add(self.operations, other.operations, "operations")?,
        })
    }

    fn is_zero(self) -> bool {
        self == Self::default()
    }
}

/// Optional limits; omitted dimensions are not limited at this boundary.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EnterpriseQuotaLimits {
    pub max_concurrent: Option<u64>,
    pub tokens: Option<u64>,
    pub provider_cost_micros: Option<u64>,
    pub worker_cost_microunits: Option<u64>,
    pub worker_runtime_millis: Option<u64>,
    pub storage_bytes: Option<u64>,
    pub operations: Option<u64>,
}

/// Immutable revision of the configured limits for one boundary.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EnterpriseQuotaPolicy {
    pub boundary: EnterpriseQuotaBoundary,
    pub revision: u64,
    pub limits: EnterpriseQuotaLimits,
}

/// Result of a policy write.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EnterpriseQuotaPolicyReceipt {
    pub policy: EnterpriseQuotaPolicy,
    pub policy_digest: String,
    pub idempotent_replay: bool,
}

/// Request to reserve enterprise allowance before operational admission.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EnterpriseQuotaReservationRequest {
    pub reservation_id: RequestId,
    pub attribution: EnterpriseUsageAttribution,
    pub source_seal: EnterpriseQuotaSourceSeal,
    pub reserved: EnterpriseQuotaAmounts,
    pub requested_at: Instant,
}

/// Exact operation identity frozen before an enterprise reservation is admitted.
///
/// The seal deliberately omits only source-catalog sequence/digest fields that
/// do not exist until the already-authoritative operation settles.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind", deny_unknown_fields)]
pub enum EnterpriseQuotaSourceSeal {
    Provider {
        model_exchange_id: ModelExchangeId,
        request_id: RequestId,
        attempt: u64,
        route_authority_fingerprint: String,
    },
    Worker {
        job_id: ExecutionJobId,
        worker_pool_id: String,
    },
    Storage {
        artifact_id: ArtifactId,
        operation_kind: crate::ArtifactStorageOperationKind,
        request_id: RequestId,
        expected_bytes: u64,
    },
    Publication {
        publication_id: PublicationId,
        operation_key: String,
        request_sha256: String,
    },
}

/// Policy revision frozen when a reservation was admitted.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EnterpriseQuotaPolicySeal {
    pub boundary: EnterpriseQuotaBoundary,
    pub revision: u64,
    pub policy_digest: String,
}

/// Terminal reason for allowance that was released without Usage.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EnterpriseQuotaReleaseReason {
    Cancelled,
    Failed,
    OperationalAdmissionDenied,
}

/// Durable reservation state.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EnterpriseQuotaReservationState {
    Active,
    Released,
    Settled,
}

impl EnterpriseQuotaReservationState {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Released => "released",
            Self::Settled => "settled",
        }
    }
}

/// Exact terminal mutation retained for replay and restart.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind", deny_unknown_fields)]
pub enum EnterpriseQuotaTerminal {
    Released {
        request_id: RequestId,
        reason: EnterpriseQuotaReleaseReason,
        released_at: Instant,
    },
    Settled {
        request_id: RequestId,
        usage_source: EnterpriseUsageSource,
        usage_sequence: u64,
        usage_source_digest: String,
        settled_at: Instant,
    },
}

/// One durable enterprise quota reservation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EnterpriseQuotaReservationRecord {
    pub reservation_id: RequestId,
    pub attribution: EnterpriseUsageAttribution,
    pub source_seal: EnterpriseQuotaSourceSeal,
    pub reserved: EnterpriseQuotaAmounts,
    pub policy_seals: Vec<EnterpriseQuotaPolicySeal>,
    pub state: EnterpriseQuotaReservationState,
    pub revision: u64,
    pub requested_at: Instant,
    pub terminal: Option<EnterpriseQuotaTerminal>,
}

/// A successful durable reservation mutation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EnterpriseQuotaReservationReceipt {
    pub record: EnterpriseQuotaReservationRecord,
    pub idempotent_replay: bool,
}

/// Quantity that caused enterprise admission to be denied.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EnterpriseQuotaDimension {
    Concurrent,
    Tokens,
    ProviderCost,
    WorkerCost,
    WorkerRuntime,
    Storage,
    Operations,
}

/// Stable, secret-safe enterprise quota denial.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EnterpriseQuotaDenial {
    pub boundary: EnterpriseQuotaBoundary,
    pub policy_revision: u64,
    pub dimension: EnterpriseQuotaDimension,
}

/// Enterprise quota admission result. A denial performs no reservation write.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EnterpriseQuotaDecision {
    Allowed(Box<EnterpriseQuotaReservationReceipt>),
    TerminalReplay(Box<EnterpriseQuotaReservationReceipt>),
    Denied(EnterpriseQuotaDenial),
}

/// Releases an active reservation without adding Usage.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EnterpriseQuotaRelease {
    pub reservation_id: RequestId,
    pub request_id: RequestId,
    pub expected_revision: u64,
    pub reason: EnterpriseQuotaReleaseReason,
    pub released_at: Instant,
}

/// Settles a reservation only from one immutable Usage ledger source.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EnterpriseQuotaSettlement {
    pub reservation_id: RequestId,
    pub request_id: RequestId,
    pub expected_revision: u64,
    pub usage_source: EnterpriseUsageSource,
}

/// Stable failure categories for quota policy and reservation operations.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EnterpriseQuotaErrorKind {
    InvalidInput,
    PolicyConflict,
    ReservationConflict,
    RevisionConflict,
    AuthorityMismatch,
    CorruptState,
    Adapter,
}

/// Quota failure without raw Usage payloads or credentials.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EnterpriseQuotaError {
    kind: EnterpriseQuotaErrorKind,
    message: String,
}

impl EnterpriseQuotaError {
    fn invalid(message: impl Into<String>) -> Self {
        Self::new(EnterpriseQuotaErrorKind::InvalidInput, message)
    }

    fn policy_conflict(message: impl Into<String>) -> Self {
        Self::new(EnterpriseQuotaErrorKind::PolicyConflict, message)
    }

    fn reservation_conflict(message: impl Into<String>) -> Self {
        Self::new(EnterpriseQuotaErrorKind::ReservationConflict, message)
    }

    fn revision_conflict(message: impl Into<String>) -> Self {
        Self::new(EnterpriseQuotaErrorKind::RevisionConflict, message)
    }

    fn authority_mismatch(message: impl Into<String>) -> Self {
        Self::new(EnterpriseQuotaErrorKind::AuthorityMismatch, message)
    }

    fn corrupt(message: impl Into<String>) -> Self {
        Self::new(EnterpriseQuotaErrorKind::CorruptState, message)
    }

    fn adapter(message: impl Into<String>) -> Self {
        Self::new(EnterpriseQuotaErrorKind::Adapter, message)
    }

    fn new(kind: EnterpriseQuotaErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    #[must_use]
    pub const fn kind(&self) -> EnterpriseQuotaErrorKind {
        self.kind
    }
}

impl fmt::Display for EnterpriseQuotaError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for EnterpriseQuotaError {}

/// `SQLite`-backed enterprise policy and reservation ledger.
pub struct EnterpriseQuotaLedger<'storage> {
    storage: &'storage mut SqliteStorage,
}

impl SqliteStorage {
    /// Opens the enterprise quota ledger on this storage connection.
    ///
    /// # Errors
    ///
    /// Returns an adapter error when quota or Usage tables cannot be prepared.
    pub fn enterprise_quota_ledger(
        &mut self,
    ) -> Result<EnterpriseQuotaLedger<'_>, EnterpriseQuotaError> {
        EnterpriseQuotaLedger::new(self)
    }
}

impl<'storage> EnterpriseQuotaLedger<'storage> {
    fn new(storage: &'storage mut SqliteStorage) -> Result<Self, EnterpriseQuotaError> {
        storage
            .enterprise_usage_ledger()
            .map_err(|error| usage_error(&error))?;
        storage
            .connection()
            .map_err(|error| storage_error(&error))?
            .execute_batch(QUOTA_SCHEMA)
            .map_err(|error| sql_error(&error))?;
        Ok(Self { storage })
    }

    /// Creates the next policy revision or returns the exact current revision.
    ///
    /// # Errors
    ///
    /// Rejects malformed policies, revision gaps, changed revision reuse,
    /// corrupt rows, and `SQLite` failures.
    pub fn put_policy(
        &mut self,
        policy: &EnterpriseQuotaPolicy,
    ) -> Result<EnterpriseQuotaPolicyReceipt, EnterpriseQuotaError> {
        validate_policy(policy)?;
        let key = boundary_key(&policy.boundary)?;
        let policy_digest = digest(policy)?;
        let boundary_json = encode(&policy.boundary)?;
        let limits_json = encode(&policy.limits)?;
        let transaction = self.transaction()?;
        let current = load_policy(&transaction, &key)?;
        if let Some(current) = current {
            if current.policy.revision == policy.revision {
                if current.policy == *policy && current.policy_digest == policy_digest {
                    transaction.commit().map_err(|error| sql_error(&error))?;
                    return Ok(EnterpriseQuotaPolicyReceipt {
                        policy: current.policy,
                        policy_digest,
                        idempotent_replay: true,
                    });
                }
                return Err(EnterpriseQuotaError::policy_conflict(
                    "enterprise quota policy revision already has different limits",
                ));
            }
            let next = current.policy.revision.checked_add(1).ok_or_else(|| {
                EnterpriseQuotaError::invalid("enterprise quota policy revision overflows")
            })?;
            if policy.revision != next {
                return Err(EnterpriseQuotaError::revision_conflict(
                    "enterprise quota policy revision is not the next durable revision",
                ));
            }
            transaction
                .execute(
                    "UPDATE enterprise_quota_policies
                     SET boundary_json = ?2, revision = ?3, limits_json = ?4, policy_digest = ?5
                     WHERE boundary_key = ?1",
                    params![
                        key,
                        boundary_json,
                        sql_integer(policy.revision)?,
                        limits_json,
                        policy_digest
                    ],
                )
                .map_err(|error| sql_error(&error))?;
        } else {
            if policy.revision != 1 {
                return Err(EnterpriseQuotaError::revision_conflict(
                    "first enterprise quota policy revision must be 1",
                ));
            }
            transaction
                .execute(
                    "INSERT INTO enterprise_quota_policies
                        (boundary_key, boundary_json, revision, limits_json, policy_digest)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        key,
                        boundary_json,
                        sql_integer(policy.revision)?,
                        limits_json,
                        policy_digest
                    ],
                )
                .map_err(|error| sql_error(&error))?;
        }
        transaction.commit().map_err(|error| sql_error(&error))?;
        Ok(EnterpriseQuotaPolicyReceipt {
            policy: policy.clone(),
            policy_digest,
            idempotent_replay: false,
        })
    }

    /// Loads the current policy for one exact boundary.
    ///
    /// # Errors
    ///
    /// Rejects malformed boundaries, corrupt rows, and `SQLite` failures.
    pub fn load_policy(
        &self,
        boundary: &EnterpriseQuotaBoundary,
    ) -> Result<Option<EnterpriseQuotaPolicyReceipt>, EnterpriseQuotaError> {
        validate_boundary(boundary)?;
        load_policy(
            self.storage
                .connection()
                .map_err(|error| storage_error(&error))?,
            &boundary_key(boundary)?,
        )
    }

    /// Atomically reserves every configured applicable boundary.
    ///
    /// # Errors
    ///
    /// Rejects malformed requests, changed reservation reuse, corrupt rows,
    /// arithmetic overflow, and `SQLite` failures.
    pub fn reserve(
        &mut self,
        request: &EnterpriseQuotaReservationRequest,
    ) -> Result<EnterpriseQuotaDecision, EnterpriseQuotaError> {
        validate_reservation_request(request)?;
        let request_digest = digest(request)?;
        let transaction = self.transaction()?;
        if let Some(current) = load_reservation(&transaction, &request.reservation_id)? {
            if current.request_digest != request_digest || current.record.request() != *request {
                return Err(EnterpriseQuotaError::reservation_conflict(
                    "enterprise quota reservation identity already belongs to another request",
                ));
            }
            transaction.commit().map_err(|error| sql_error(&error))?;
            let receipt = Box::new(EnterpriseQuotaReservationReceipt {
                record: current.record,
                idempotent_replay: true,
            });
            return Ok(
                if receipt.record.state == EnterpriseQuotaReservationState::Active {
                    EnterpriseQuotaDecision::Allowed(receipt)
                } else {
                    EnterpriseQuotaDecision::TerminalReplay(receipt)
                },
            );
        }
        if load_reservation_id_by_source_seal(
            &transaction,
            &source_seal_key(&request.source_seal)?,
        )?
        .is_some()
        {
            return Err(EnterpriseQuotaError::reservation_conflict(
                "enterprise quota source authority already belongs to another reservation",
            ));
        }
        let boundaries = applicable_boundaries(&request.attribution);
        let mut seals = Vec::new();
        for boundary in &boundaries {
            let key = boundary_key(boundary)?;
            if let Some(policy) = load_policy(&transaction, &key)? {
                if let Some(denial) = check_policy(&transaction, &policy, request.reserved)? {
                    return Ok(EnterpriseQuotaDecision::Denied(denial));
                }
                seals.push(EnterpriseQuotaPolicySeal {
                    boundary: policy.policy.boundary,
                    revision: policy.policy.revision,
                    policy_digest: policy.policy_digest,
                });
            }
        }
        let record = EnterpriseQuotaReservationRecord {
            reservation_id: request.reservation_id.clone(),
            attribution: request.attribution.clone(),
            source_seal: request.source_seal.clone(),
            reserved: request.reserved,
            policy_seals: seals,
            state: EnterpriseQuotaReservationState::Active,
            revision: 1,
            requested_at: request.requested_at.clone(),
            terminal: None,
        };
        insert_reservation(&transaction, &record, &request_digest, &boundaries)?;
        transaction.commit().map_err(|error| sql_error(&error))?;
        Ok(EnterpriseQuotaDecision::Allowed(Box::new(
            EnterpriseQuotaReservationReceipt {
                record,
                idempotent_replay: false,
            },
        )))
    }

    /// Releases a reservation after cancellation, failure, or downstream denial.
    ///
    /// # Errors
    ///
    /// Rejects unknown reservations, stale revisions, changed terminal replay,
    /// corrupt rows, and `SQLite` failures.
    pub fn release(
        &mut self,
        release: &EnterpriseQuotaRelease,
    ) -> Result<EnterpriseQuotaReservationReceipt, EnterpriseQuotaError> {
        validate_release(release)?;
        let command_digest = digest(release)?;
        let transaction = self.transaction()?;
        if let Some(receipt) = load_terminal_receipt(&transaction, &release.request_id)? {
            return replay_terminal_receipt(
                transaction,
                &receipt,
                &release.reservation_id,
                "release",
                &command_digest,
            );
        }
        let current = require_reservation(&transaction, &release.reservation_id)?;
        if current.record.state != EnterpriseQuotaReservationState::Active {
            return Err(terminal_without_receipt(
                &current.record,
                &release.request_id,
            ));
        }
        require_revision_time(
            &current.record,
            release.expected_revision,
            &release.released_at,
        )?;
        let mut record = current.record;
        record.state = EnterpriseQuotaReservationState::Released;
        record.revision = next_revision(record.revision)?;
        record.terminal = Some(EnterpriseQuotaTerminal::Released {
            request_id: release.request_id.clone(),
            reason: release.reason,
            released_at: release.released_at.clone(),
        });
        update_terminal(&transaction, &record, None)?;
        insert_terminal_receipt(
            &transaction,
            &release.request_id,
            &release.reservation_id,
            "release",
            &command_digest,
            &record,
        )?;
        transaction.commit().map_err(|error| sql_error(&error))?;
        Ok(EnterpriseQuotaReservationReceipt {
            record,
            idempotent_replay: false,
        })
    }

    /// Settles a reservation from one exact immutable enterprise Usage source.
    ///
    /// # Errors
    ///
    /// Rejects missing or mismatched Usage, stale revisions, over-reservation,
    /// changed terminal replay, corrupt rows, and `SQLite` failures.
    pub fn settle(
        &mut self,
        settlement: &EnterpriseQuotaSettlement,
    ) -> Result<EnterpriseQuotaReservationReceipt, EnterpriseQuotaError> {
        validate_settlement(settlement)?;
        let command_digest = digest(settlement)?;
        let transaction = self.transaction()?;
        if let Some(receipt) = load_terminal_receipt(&transaction, &settlement.request_id)? {
            return replay_terminal_receipt(
                transaction,
                &receipt,
                &settlement.reservation_id,
                "settle",
                &command_digest,
            );
        }
        let current = require_reservation(&transaction, &settlement.reservation_id)?;
        if current.record.state != EnterpriseQuotaReservationState::Active {
            return Err(terminal_without_receipt(
                &current.record,
                &settlement.request_id,
            ));
        }
        if current.record.revision != settlement.expected_revision {
            return Err(EnterpriseQuotaError::revision_conflict(
                "enterprise quota reservation revision is stale",
            ));
        }
        let usage = crate::enterprise_usage::load_quota_usage_source(
            &transaction,
            &settlement.usage_source,
        )
        .map_err(|error| usage_error(&error))?
        .ok_or_else(|| {
            EnterpriseQuotaError::authority_mismatch(
                "enterprise quota settlement Usage source is not durable",
            )
        })?;
        if usage.fact.source != settlement.usage_source {
            return Err(EnterpriseQuotaError::authority_mismatch(
                "enterprise quota settlement source differs from its durable fact",
            ));
        }
        validate_usage_authority(&current.record, &usage)?;
        let actual = amounts_from_entry(&usage)?;
        require_within_reserved(actual, current.record.reserved)?;
        let mut record = current.record;
        record.state = EnterpriseQuotaReservationState::Settled;
        record.revision = next_revision(record.revision)?;
        record.terminal = Some(EnterpriseQuotaTerminal::Settled {
            request_id: settlement.request_id.clone(),
            usage_source: usage.fact.source.clone(),
            usage_sequence: usage.sequence,
            usage_source_digest: usage.source_digest,
            settled_at: usage.fact.settled_at,
        });
        update_terminal(&transaction, &record, Some(usage.sequence))?;
        insert_terminal_receipt(
            &transaction,
            &settlement.request_id,
            &settlement.reservation_id,
            "settle",
            &command_digest,
            &record,
        )?;
        transaction.commit().map_err(|error| sql_error(&error))?;
        Ok(EnterpriseQuotaReservationReceipt {
            record,
            idempotent_replay: false,
        })
    }

    /// Settles the reservation sealed to one immutable Usage source.
    ///
    /// This recovery path is used by bounded source reconcilers after they
    /// have projected a terminal Provider, Worker, storage, or Publication
    /// fact into the canonical enterprise Usage ledger. Sources without a
    /// matching reservation are historical/unmetered facts and return
    /// `None`; a matching released or corrupt reservation fails closed.
    ///
    /// # Errors
    ///
    /// Rejects corrupt Usage, ambiguous/mismatched reservation authority,
    /// changed terminal state, arithmetic overflow, or unavailable storage.
    pub fn settle_usage_source(
        &mut self,
        usage_source: &EnterpriseUsageSource,
    ) -> Result<Option<EnterpriseQuotaReservationReceipt>, EnterpriseQuotaError> {
        let transaction = self.transaction()?;
        let usage = crate::enterprise_usage::load_quota_usage_source(&transaction, usage_source)
            .map_err(|error| usage_error(&error))?
            .ok_or_else(|| {
                EnterpriseQuotaError::authority_mismatch(
                    "enterprise quota recovery Usage source is not durable",
                )
            })?;
        if usage.fact.source != *usage_source {
            return Err(EnterpriseQuotaError::authority_mismatch(
                "enterprise quota recovery source differs from its durable fact",
            ));
        }
        let source_seal = source_seal_from_usage(&usage)?;
        let source_key = source_seal_key(&source_seal)?;
        let Some(reservation_id) = load_reservation_id_by_source_seal(&transaction, &source_key)?
        else {
            transaction.commit().map_err(|error| sql_error(&error))?;
            return Ok(None);
        };
        let current = require_reservation(&transaction, &reservation_id)?;
        validate_usage_authority(&current.record, &usage)?;
        require_within_reserved(amounts_from_entry(&usage)?, current.record.reserved)?;
        match current.record.state {
            EnterpriseQuotaReservationState::Active => {
                let request_id = quota_settlement_request_id(&source_key);
                let settlement = EnterpriseQuotaSettlement {
                    reservation_id,
                    request_id: request_id.clone(),
                    expected_revision: current.record.revision,
                    usage_source: usage.fact.source.clone(),
                };
                let command_digest = digest(&settlement)?;
                let mut record = current.record;
                record.state = EnterpriseQuotaReservationState::Settled;
                record.revision = next_revision(record.revision)?;
                record.terminal = Some(EnterpriseQuotaTerminal::Settled {
                    request_id: request_id.clone(),
                    usage_source: usage.fact.source,
                    usage_sequence: usage.sequence,
                    usage_source_digest: usage.source_digest,
                    settled_at: usage.fact.settled_at,
                });
                update_terminal(&transaction, &record, Some(usage.sequence))?;
                insert_terminal_receipt(
                    &transaction,
                    &request_id,
                    &record.reservation_id,
                    "settle",
                    &command_digest,
                    &record,
                )?;
                transaction.commit().map_err(|error| sql_error(&error))?;
                Ok(Some(EnterpriseQuotaReservationReceipt {
                    record,
                    idempotent_replay: false,
                }))
            }
            EnterpriseQuotaReservationState::Settled => {
                transaction.commit().map_err(|error| sql_error(&error))?;
                Ok(Some(EnterpriseQuotaReservationReceipt {
                    record: current.record,
                    idempotent_replay: true,
                }))
            }
            EnterpriseQuotaReservationState::Released => {
                Err(EnterpriseQuotaError::reservation_conflict(
                    "enterprise quota Usage settled after its reservation was released",
                ))
            }
        }
    }

    /// Loads one exact durable reservation.
    ///
    /// # Errors
    ///
    /// Rejects malformed identities, corrupt rows, and `SQLite` failures.
    pub fn load_reservation(
        &self,
        reservation_id: &RequestId,
    ) -> Result<Option<EnterpriseQuotaReservationRecord>, EnterpriseQuotaError> {
        validate_id(&reservation_id.0, "req_", "reservationId")?;
        load_reservation(
            self.storage
                .connection()
                .map_err(|error| storage_error(&error))?,
            reservation_id,
        )
        .map(|value| value.map(|stored| stored.record))
    }

    fn transaction(&mut self) -> Result<Transaction<'_>, EnterpriseQuotaError> {
        self.storage
            .connection_mut()
            .map_err(|error| storage_error(&error))?
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| sql_error(&error))
    }
}

impl EnterpriseQuotaReservationRecord {
    fn request(&self) -> EnterpriseQuotaReservationRequest {
        EnterpriseQuotaReservationRequest {
            reservation_id: self.reservation_id.clone(),
            attribution: self.attribution.clone(),
            source_seal: self.source_seal.clone(),
            reserved: self.reserved,
            requested_at: self.requested_at.clone(),
        }
    }
}

#[derive(Clone)]
struct StoredReservation {
    request_digest: String,
    record: EnterpriseQuotaReservationRecord,
}

fn insert_reservation(
    transaction: &Transaction<'_>,
    record: &EnterpriseQuotaReservationRecord,
    request_digest: &str,
    boundaries: &[EnterpriseQuotaBoundary],
) -> Result<(), EnterpriseQuotaError> {
    transaction
        .execute(
            "INSERT INTO enterprise_quota_reservations
                (reservation_id, request_digest, record_json, state, revision,
                 reserved_tokens, reserved_provider_cost_micros,
                 reserved_worker_cost_microunits,
                 reserved_worker_runtime_millis, reserved_storage_bytes,
                 reserved_operations, source_seal_key, settled_usage_sequence)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, NULL)",
            params![
                record.reservation_id.0,
                request_digest,
                encode(record)?,
                record.state.as_str(),
                sql_integer(record.revision)?,
                sql_integer(record.reserved.tokens)?,
                sql_integer(record.reserved.provider_cost_micros)?,
                sql_integer(record.reserved.worker_cost_microunits)?,
                sql_integer(record.reserved.worker_runtime_millis)?,
                sql_integer(record.reserved.storage_bytes)?,
                sql_integer(record.reserved.operations)?,
                source_seal_key(&record.source_seal)?,
            ],
        )
        .map_err(|error| sql_error(&error))?;
    for (ordinal, boundary) in boundaries.iter().enumerate() {
        transaction
            .execute(
                "INSERT INTO enterprise_quota_reservation_boundaries
                    (reservation_id, boundary_key, ordinal) VALUES (?1, ?2, ?3)",
                params![
                    record.reservation_id.0,
                    boundary_key(boundary)?,
                    i64::try_from(ordinal).map_err(|_| {
                        EnterpriseQuotaError::invalid("quota boundary ordinal overflows")
                    })?
                ],
            )
            .map_err(|error| sql_error(&error))?;
    }
    Ok(())
}

fn update_terminal(
    transaction: &Transaction<'_>,
    record: &EnterpriseQuotaReservationRecord,
    settled_usage_sequence: Option<u64>,
) -> Result<(), EnterpriseQuotaError> {
    let changed = transaction
        .execute(
            "UPDATE enterprise_quota_reservations
             SET record_json = ?2, state = ?3, revision = ?4, settled_usage_sequence = ?5
             WHERE reservation_id = ?1 AND state = 'active'",
            params![
                record.reservation_id.0,
                encode(record)?,
                record.state.as_str(),
                sql_integer(record.revision)?,
                settled_usage_sequence.map(sql_integer).transpose()?,
            ],
        )
        .map_err(|error| sql_error(&error))?;
    if changed != 1 {
        return Err(EnterpriseQuotaError::revision_conflict(
            "enterprise quota reservation changed during terminal mutation",
        ));
    }
    Ok(())
}

struct StoredTerminalReceipt {
    reservation_id: RequestId,
    command_kind: String,
    command_digest: String,
    terminal_revision: u64,
    record_digest: String,
}

fn load_terminal_receipt(
    connection: &rusqlite::Connection,
    request_id: &RequestId,
) -> Result<Option<StoredTerminalReceipt>, EnterpriseQuotaError> {
    connection
        .query_row(
            "SELECT reservation_id, command_kind, command_digest,
                    terminal_revision, record_digest
             FROM enterprise_quota_terminal_receipts WHERE request_id = ?1",
            [&request_id.0],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, String>(4)?,
                ))
            },
        )
        .optional()
        .map_err(|error| sql_error(&error))?
        .map(
            |(reservation_id, command_kind, command_digest, terminal_revision, record_digest)| {
                let terminal_revision = u64::try_from(terminal_revision).map_err(|_| {
                    EnterpriseQuotaError::corrupt(
                        "stored quota terminal receipt revision is invalid",
                    )
                })?;
                validate_id(&reservation_id, "req_", "storedReservationId").map_err(|_| {
                    EnterpriseQuotaError::corrupt("stored quota receipt is invalid")
                })?;
                if !matches!(command_kind.as_str(), "release" | "settle") {
                    return Err(EnterpriseQuotaError::corrupt(
                        "stored quota terminal receipt kind is invalid",
                    ));
                }
                validate_digest(&command_digest, "commandDigest")?;
                validate_digest(&record_digest, "recordDigest")?;
                positive_safe(terminal_revision, "terminalRevision")?;
                Ok(StoredTerminalReceipt {
                    reservation_id: RequestId(reservation_id),
                    command_kind,
                    command_digest,
                    terminal_revision,
                    record_digest,
                })
            },
        )
        .transpose()
}

fn insert_terminal_receipt(
    transaction: &Transaction<'_>,
    request_id: &RequestId,
    reservation_id: &RequestId,
    command_kind: &str,
    command_digest: &str,
    record: &EnterpriseQuotaReservationRecord,
) -> Result<(), EnterpriseQuotaError> {
    transaction
        .execute(
            "INSERT INTO enterprise_quota_terminal_receipts
                (request_id, reservation_id, command_kind, command_digest,
                 terminal_revision, record_digest)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                request_id.0,
                reservation_id.0,
                command_kind,
                command_digest,
                sql_integer(record.revision)?,
                digest(record)?,
            ],
        )
        .map_err(|error| sql_error(&error))?;
    Ok(())
}

fn replay_terminal_receipt(
    transaction: Transaction<'_>,
    receipt: &StoredTerminalReceipt,
    reservation_id: &RequestId,
    command_kind: &str,
    command_digest: &str,
) -> Result<EnterpriseQuotaReservationReceipt, EnterpriseQuotaError> {
    if receipt.reservation_id != *reservation_id
        || receipt.command_kind != command_kind
        || receipt.command_digest != command_digest
    {
        return Err(EnterpriseQuotaError::reservation_conflict(
            "enterprise quota terminal request identity was reused",
        ));
    }
    let record = require_reservation(&transaction, reservation_id)?.record;
    if record.revision != receipt.terminal_revision || digest(&record)? != receipt.record_digest {
        return Err(EnterpriseQuotaError::corrupt(
            "stored quota terminal receipt differs from its reservation",
        ));
    }
    if let EnterpriseQuotaTerminal::Settled { usage_source, .. } =
        record.terminal.as_ref().ok_or_else(|| {
            EnterpriseQuotaError::corrupt("quota terminal receipt points to an active reservation")
        })?
    {
        let usage = crate::enterprise_usage::load_quota_usage_source(&transaction, usage_source)
            .map_err(|error| usage_error(&error))?
            .ok_or_else(|| {
                EnterpriseQuotaError::corrupt("settled quota reservation lost its Usage authority")
            })?;
        validate_terminal_usage(&record, &usage)?;
    }
    transaction.commit().map_err(|error| sql_error(&error))?;
    Ok(EnterpriseQuotaReservationReceipt {
        record,
        idempotent_replay: true,
    })
}

fn load_reservation(
    connection: &rusqlite::Connection,
    reservation_id: &RequestId,
) -> Result<Option<StoredReservation>, EnterpriseQuotaError> {
    let stored = connection
        .query_row(
            "SELECT request_digest, record_json, state, revision,
                    reserved_tokens, reserved_provider_cost_micros,
                    reserved_worker_cost_microunits,
                    reserved_worker_runtime_millis, reserved_storage_bytes,
                    reserved_operations, source_seal_key, settled_usage_sequence
             FROM enterprise_quota_reservations WHERE reservation_id = ?1",
            [&reservation_id.0],
            |row| {
                Ok(StoredReservationRow {
                    request_digest: row.get(0)?,
                    record_json: row.get(1)?,
                    state: row.get(2)?,
                    revision: row.get(3)?,
                    amounts: SqlAmounts {
                        tokens: row.get(4)?,
                        provider_cost_micros: row.get(5)?,
                        worker_cost_microunits: row.get(6)?,
                        worker_runtime_millis: row.get(7)?,
                        storage_bytes: row.get(8)?,
                        operations: row.get(9)?,
                    },
                    source_seal_key: row.get(10)?,
                    settled_usage_sequence: row.get(11)?,
                })
            },
        )
        .optional()
        .map_err(|error| sql_error(&error))?;
    stored
        .map(|row| complete_stored_reservation(connection, reservation_id, row))
        .transpose()
}

fn load_reservation_id_by_source_seal(
    connection: &rusqlite::Connection,
    seal_key: &str,
) -> Result<Option<RequestId>, EnterpriseQuotaError> {
    connection
        .query_row(
            "SELECT reservation_id FROM enterprise_quota_reservations
             WHERE source_seal_key = ?1",
            [seal_key],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map(|value| value.map(RequestId))
        .map_err(|error| sql_error(&error))
}

struct StoredReservationRow {
    request_digest: String,
    record_json: String,
    state: String,
    revision: i64,
    amounts: SqlAmounts,
    source_seal_key: String,
    settled_usage_sequence: Option<i64>,
}

#[derive(Clone, Copy)]
struct SqlAmounts {
    tokens: i64,
    provider_cost_micros: i64,
    worker_cost_microunits: i64,
    worker_runtime_millis: i64,
    storage_bytes: i64,
    operations: i64,
}

fn complete_stored_reservation(
    connection: &rusqlite::Connection,
    reservation_id: &RequestId,
    stored: StoredReservationRow,
) -> Result<StoredReservation, EnterpriseQuotaError> {
    let StoredReservationRow {
        request_digest: stored_request_digest,
        record_json,
        state,
        revision,
        amounts,
        source_seal_key: stored_source_seal_key,
        settled_usage_sequence: stored_usage_sequence,
    } = stored;
    let record: EnterpriseQuotaReservationRecord = serde_json::from_str(&record_json)
        .map_err(|_| EnterpriseQuotaError::corrupt("stored quota reservation JSON is invalid"))?;
    validate_record(&record)?;
    let canonical_json = encode(&record)?;
    let request_digest = digest(&record.request())?;
    let expected_sequence = settled_usage_sequence(&record);
    if canonical_json != record_json
        || record.reservation_id != *reservation_id
        || record.state.as_str() != state
        || sql_integer(record.revision)? != revision
        || SqlAmounts::from_amounts(record.reserved)? != amounts
        || source_seal_key(&record.source_seal)? != stored_source_seal_key
        || stored_usage_sequence != expected_sequence.map(sql_integer).transpose()?
        || stored_request_digest != request_digest
    {
        return Err(EnterpriseQuotaError::corrupt(
            "stored enterprise quota reservation differs from its canonical record",
        ));
    }
    validate_boundary_links(connection, &record)?;
    validate_terminal_receipt_link(connection, &record)?;
    Ok(StoredReservation {
        request_digest,
        record,
    })
}

impl PartialEq for SqlAmounts {
    fn eq(&self, other: &Self) -> bool {
        self.tokens == other.tokens
            && self.provider_cost_micros == other.provider_cost_micros
            && self.worker_cost_microunits == other.worker_cost_microunits
            && self.worker_runtime_millis == other.worker_runtime_millis
            && self.storage_bytes == other.storage_bytes
            && self.operations == other.operations
    }
}

impl SqlAmounts {
    fn from_amounts(amounts: EnterpriseQuotaAmounts) -> Result<Self, EnterpriseQuotaError> {
        Ok(Self {
            tokens: sql_integer(amounts.tokens)?,
            provider_cost_micros: sql_integer(amounts.provider_cost_micros)?,
            worker_cost_microunits: sql_integer(amounts.worker_cost_microunits)?,
            worker_runtime_millis: sql_integer(amounts.worker_runtime_millis)?,
            storage_bytes: sql_integer(amounts.storage_bytes)?,
            operations: sql_integer(amounts.operations)?,
        })
    }
}

fn validate_boundary_links(
    connection: &rusqlite::Connection,
    record: &EnterpriseQuotaReservationRecord,
) -> Result<(), EnterpriseQuotaError> {
    let mut statement = connection
        .prepare(
            "SELECT boundary_key FROM enterprise_quota_reservation_boundaries
             WHERE reservation_id = ?1 ORDER BY ordinal",
        )
        .map_err(|error| sql_error(&error))?;
    let stored = statement
        .query_map([&record.reservation_id.0], |row| row.get::<_, String>(0))
        .map_err(|error| sql_error(&error))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| sql_error(&error))?;
    let expected = applicable_boundaries(&record.attribution)
        .iter()
        .map(boundary_key)
        .collect::<Result<Vec<_>, _>>()?;
    if stored != expected {
        return Err(EnterpriseQuotaError::corrupt(
            "stored enterprise quota boundary links are incomplete",
        ));
    }
    Ok(())
}

fn settled_usage_sequence(record: &EnterpriseQuotaReservationRecord) -> Option<u64> {
    match &record.terminal {
        Some(EnterpriseQuotaTerminal::Settled { usage_sequence, .. }) => Some(*usage_sequence),
        Some(EnterpriseQuotaTerminal::Released { .. }) | None => None,
    }
}

fn validate_terminal_receipt_link(
    connection: &rusqlite::Connection,
    record: &EnterpriseQuotaReservationRecord,
) -> Result<(), EnterpriseQuotaError> {
    let mut statement = connection
        .prepare(
            "SELECT request_id FROM enterprise_quota_terminal_receipts
             WHERE reservation_id = ?1",
        )
        .map_err(|error| sql_error(&error))?;
    let request_ids = statement
        .query_map([&record.reservation_id.0], |row| row.get::<_, String>(0))
        .map_err(|error| sql_error(&error))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| sql_error(&error))?;
    match (&record.terminal, request_ids.as_slice()) {
        (None, []) => Ok(()),
        (
            Some(EnterpriseQuotaTerminal::Released {
                request_id,
                reason,
                released_at,
            }),
            [stored_request_id],
        ) => {
            let command = EnterpriseQuotaRelease {
                reservation_id: record.reservation_id.clone(),
                request_id: request_id.clone(),
                expected_revision: record.revision.checked_sub(1).ok_or_else(|| {
                    EnterpriseQuotaError::corrupt("quota terminal revision underflows")
                })?,
                reason: *reason,
                released_at: released_at.clone(),
            };
            validate_receipt_link(
                connection,
                record,
                request_id,
                stored_request_id,
                "release",
                &digest(&command)?,
            )
        }
        (
            Some(EnterpriseQuotaTerminal::Settled {
                request_id,
                usage_source,
                ..
            }),
            [stored_request_id],
        ) => {
            let command = EnterpriseQuotaSettlement {
                reservation_id: record.reservation_id.clone(),
                request_id: request_id.clone(),
                expected_revision: record.revision.checked_sub(1).ok_or_else(|| {
                    EnterpriseQuotaError::corrupt("quota terminal revision underflows")
                })?,
                usage_source: usage_source.clone(),
            };
            validate_receipt_link(
                connection,
                record,
                request_id,
                stored_request_id,
                "settle",
                &digest(&command)?,
            )
        }
        _ => Err(EnterpriseQuotaError::corrupt(
            "quota reservation terminal receipt cardinality is inconsistent",
        )),
    }
}

fn validate_receipt_link(
    connection: &rusqlite::Connection,
    record: &EnterpriseQuotaReservationRecord,
    request_id: &RequestId,
    stored_request_id: &str,
    command_kind: &str,
    command_digest: &str,
) -> Result<(), EnterpriseQuotaError> {
    if stored_request_id != request_id.0 {
        return Err(EnterpriseQuotaError::corrupt(
            "quota reservation points to another terminal request",
        ));
    }
    let receipt = load_terminal_receipt(connection, request_id)?.ok_or_else(|| {
        EnterpriseQuotaError::corrupt("quota reservation lost its terminal receipt")
    })?;
    if receipt.reservation_id != record.reservation_id
        || receipt.command_kind != command_kind
        || receipt.command_digest != command_digest
        || receipt.terminal_revision != record.revision
        || receipt.record_digest != digest(record)?
    {
        return Err(EnterpriseQuotaError::corrupt(
            "quota terminal receipt differs from its reservation",
        ));
    }
    Ok(())
}

fn require_reservation(
    connection: &rusqlite::Connection,
    reservation_id: &RequestId,
) -> Result<StoredReservation, EnterpriseQuotaError> {
    load_reservation(connection, reservation_id)?.ok_or_else(|| {
        EnterpriseQuotaError::authority_mismatch("enterprise quota reservation does not exist")
    })
}

fn check_policy(
    transaction: &Transaction<'_>,
    policy: &EnterpriseQuotaPolicyReceipt,
    requested: EnterpriseQuotaAmounts,
) -> Result<Option<EnterpriseQuotaDenial>, EnterpriseQuotaError> {
    let boundary_key = boundary_key(&policy.policy.boundary)?;
    let active = active_usage(transaction, &boundary_key)?;
    let settled = crate::enterprise_usage::reconcile_totals(
        transaction,
        &boundary_filter(&policy.policy.boundary),
    )
    .map_err(|error| usage_error(&error))?;
    let total = amounts_from_totals(settled)?
        .checked_add(active.amounts)?
        .checked_add(requested)?;
    let concurrent = active
        .concurrent
        .checked_add(1)
        .ok_or_else(|| EnterpriseQuotaError::corrupt("active reservation count overflows"))?;
    let dimension = exceeded_dimension(concurrent, total, policy.policy.limits);
    Ok(dimension.map(|dimension| EnterpriseQuotaDenial {
        boundary: policy.policy.boundary.clone(),
        policy_revision: policy.policy.revision,
        dimension,
    }))
}

#[derive(Clone, Copy, Default)]
struct ActiveUsage {
    concurrent: u64,
    amounts: EnterpriseQuotaAmounts,
}

fn active_usage(
    connection: &rusqlite::Connection,
    boundary_key: &str,
) -> Result<ActiveUsage, EnterpriseQuotaError> {
    let mut statement = connection
        .prepare(
            "SELECT r.reservation_id
             FROM enterprise_quota_reservations r
             JOIN enterprise_quota_reservation_boundaries b
               ON b.reservation_id = r.reservation_id
             WHERE b.boundary_key = ?1 AND r.state = 'active'
             ORDER BY r.reservation_id
             LIMIT ?2",
        )
        .map_err(|error| sql_error(&error))?;
    let limit = sql_integer(MAX_ACTIVE_RESERVATIONS_PER_BOUNDARY + 1)?;
    let ids = statement
        .query_map(params![boundary_key, limit], |row| row.get::<_, String>(0))
        .map_err(|error| sql_error(&error))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| sql_error(&error))?;
    if u64::try_from(ids.len()).unwrap_or(u64::MAX) > MAX_ACTIVE_RESERVATIONS_PER_BOUNDARY {
        return Err(EnterpriseQuotaError::corrupt(
            "active enterprise quota reservation scan exceeds its bound",
        ));
    }
    let mut usage = ActiveUsage::default();
    for id in ids {
        let request_id = RequestId(id);
        let reservation = require_reservation(connection, &request_id)?;
        if reservation.record.state != EnterpriseQuotaReservationState::Active {
            return Err(EnterpriseQuotaError::corrupt(
                "active quota index points to a terminal reservation",
            ));
        }
        usage.concurrent = usage
            .concurrent
            .checked_add(1)
            .ok_or_else(|| EnterpriseQuotaError::corrupt("active reservation count overflows"))?;
        usage.amounts = usage.amounts.checked_add(reservation.record.reserved)?;
    }
    Ok(usage)
}

fn exceeded_dimension(
    concurrent: u64,
    amounts: EnterpriseQuotaAmounts,
    limits: EnterpriseQuotaLimits,
) -> Option<EnterpriseQuotaDimension> {
    [
        (
            limits
                .max_concurrent
                .is_some_and(|limit| concurrent > limit),
            EnterpriseQuotaDimension::Concurrent,
        ),
        (
            limits.tokens.is_some_and(|limit| amounts.tokens > limit),
            EnterpriseQuotaDimension::Tokens,
        ),
        (
            limits
                .provider_cost_micros
                .is_some_and(|limit| amounts.provider_cost_micros > limit),
            EnterpriseQuotaDimension::ProviderCost,
        ),
        (
            limits
                .worker_cost_microunits
                .is_some_and(|limit| amounts.worker_cost_microunits > limit),
            EnterpriseQuotaDimension::WorkerCost,
        ),
        (
            limits
                .worker_runtime_millis
                .is_some_and(|limit| amounts.worker_runtime_millis > limit),
            EnterpriseQuotaDimension::WorkerRuntime,
        ),
        (
            limits
                .storage_bytes
                .is_some_and(|limit| amounts.storage_bytes > limit),
            EnterpriseQuotaDimension::Storage,
        ),
        (
            limits
                .operations
                .is_some_and(|limit| amounts.operations > limit),
            EnterpriseQuotaDimension::Operations,
        ),
    ]
    .into_iter()
    .find_map(|(exceeded, dimension)| exceeded.then_some(dimension))
}

fn terminal_without_receipt(
    record: &EnterpriseQuotaReservationRecord,
    request_id: &RequestId,
) -> EnterpriseQuotaError {
    let stored_request_id = match record.terminal.as_ref() {
        Some(
            EnterpriseQuotaTerminal::Released { request_id, .. }
            | EnterpriseQuotaTerminal::Settled { request_id, .. },
        ) => Some(request_id),
        None => None,
    };
    if stored_request_id == Some(request_id) {
        EnterpriseQuotaError::corrupt("terminal quota reservation lost its durable command receipt")
    } else {
        EnterpriseQuotaError::reservation_conflict(
            "enterprise quota reservation already has another terminal mutation",
        )
    }
}

fn validate_terminal_usage(
    record: &EnterpriseQuotaReservationRecord,
    usage: &EnterpriseUsageEntry,
) -> Result<(), EnterpriseQuotaError> {
    let Some(EnterpriseQuotaTerminal::Settled {
        usage_source,
        usage_sequence,
        usage_source_digest,
        settled_at,
        ..
    }) = &record.terminal
    else {
        return Err(EnterpriseQuotaError::corrupt(
            "settled quota reservation has no terminal Usage authority",
        ));
    };
    validate_usage_authority(record, usage)?;
    if usage.fact.source != *usage_source
        || usage.sequence != *usage_sequence
        || usage.source_digest != *usage_source_digest
        || usage.fact.settled_at != *settled_at
    {
        return Err(EnterpriseQuotaError::corrupt(
            "settled quota reservation differs from its durable Usage authority",
        ));
    }
    Ok(())
}

fn require_revision_time(
    record: &EnterpriseQuotaReservationRecord,
    expected_revision: u64,
    occurred_at: &Instant,
) -> Result<(), EnterpriseQuotaError> {
    if record.revision != expected_revision {
        return Err(EnterpriseQuotaError::revision_conflict(
            "enterprise quota reservation revision is stale",
        ));
    }
    if occurred_at.0 < record.requested_at.0 {
        return Err(EnterpriseQuotaError::invalid(
            "enterprise quota terminal time precedes reservation",
        ));
    }
    Ok(())
}

fn validate_usage_authority(
    reservation: &EnterpriseQuotaReservationRecord,
    usage: &EnterpriseUsageEntry,
) -> Result<(), EnterpriseQuotaError> {
    if usage.fact.attribution != reservation.attribution
        || !source_seal_matches(&reservation.source_seal, &usage.fact.source)
    {
        return Err(EnterpriseQuotaError::authority_mismatch(
            "enterprise quota settlement Usage does not match reserved authority",
        ));
    }
    if let (
        EnterpriseQuotaSourceSeal::Storage { expected_bytes, .. },
        crate::EnterpriseUsageMeasure::Storage { bytes },
    ) = (&reservation.source_seal, &usage.fact.measure)
        && expected_bytes != bytes
    {
        return Err(EnterpriseQuotaError::authority_mismatch(
            "enterprise quota Storage usage differs from its open authority",
        ));
    }
    if usage.fact.settled_at.0 < reservation.requested_at.0 {
        return Err(EnterpriseQuotaError::authority_mismatch(
            "enterprise quota settlement predates the reservation",
        ));
    }
    Ok(())
}

fn source_seal_matches(seal: &EnterpriseQuotaSourceSeal, source: &EnterpriseUsageSource) -> bool {
    match (seal, source) {
        (
            EnterpriseQuotaSourceSeal::Provider {
                model_exchange_id,
                request_id,
                attempt,
                route_authority_fingerprint,
            },
            EnterpriseUsageSource::Provider {
                model_exchange_id: source_exchange_id,
                request_id: source_request_id,
                attempt: source_attempt,
                route_authority_fingerprint: source_fingerprint,
                ..
            },
        ) => {
            model_exchange_id == source_exchange_id
                && request_id == source_request_id
                && attempt == source_attempt
                && route_authority_fingerprint == source_fingerprint
        }
        (
            EnterpriseQuotaSourceSeal::Worker {
                job_id,
                worker_pool_id,
            },
            EnterpriseUsageSource::Worker {
                job_id: source_job_id,
                worker_pool_id: source_pool_id,
                ..
            },
        ) => job_id == source_job_id && worker_pool_id == source_pool_id,
        (
            EnterpriseQuotaSourceSeal::Storage {
                artifact_id,
                operation_kind,
                request_id,
                ..
            },
            EnterpriseUsageSource::Storage {
                artifact_id: source_artifact_id,
                operation_kind: source_operation_kind,
                request_id: source_request_id,
                ..
            },
        ) => {
            artifact_id == source_artifact_id
                && operation_kind == source_operation_kind
                && request_id == source_request_id
        }
        (
            EnterpriseQuotaSourceSeal::Publication {
                publication_id,
                operation_key,
                request_sha256,
            },
            EnterpriseUsageSource::Publication {
                publication_id: source_publication_id,
                operation_key: source_operation_key,
                request_sha256: source_request_sha256,
            },
        ) => {
            publication_id == source_publication_id
                && operation_key == source_operation_key
                && request_sha256 == source_request_sha256
        }
        _ => false,
    }
}

fn require_within_reserved(
    actual: EnterpriseQuotaAmounts,
    reserved: EnterpriseQuotaAmounts,
) -> Result<(), EnterpriseQuotaError> {
    if actual.tokens > reserved.tokens
        || actual.provider_cost_micros > reserved.provider_cost_micros
        || actual.worker_cost_microunits > reserved.worker_cost_microunits
        || actual.worker_runtime_millis > reserved.worker_runtime_millis
        || actual.storage_bytes > reserved.storage_bytes
        || actual.operations > reserved.operations
    {
        return Err(EnterpriseQuotaError::authority_mismatch(
            "settled enterprise Usage exceeds its reservation",
        ));
    }
    Ok(())
}

fn amounts_from_entry(
    entry: &EnterpriseUsageEntry,
) -> Result<EnterpriseQuotaAmounts, EnterpriseQuotaError> {
    use crate::EnterpriseUsageMeasure;
    let amounts = match entry.fact.measure {
        EnterpriseUsageMeasure::Provider {
            total_tokens,
            cost_micros,
            ..
        } => EnterpriseQuotaAmounts {
            tokens: total_tokens,
            provider_cost_micros: cost_micros,
            operations: 1,
            ..EnterpriseQuotaAmounts::default()
        },
        EnterpriseUsageMeasure::Worker {
            runtime_millis,
            tokens,
            cost_microunits,
        } => EnterpriseQuotaAmounts {
            tokens,
            worker_cost_microunits: cost_microunits,
            worker_runtime_millis: runtime_millis,
            operations: 1,
            ..EnterpriseQuotaAmounts::default()
        },
        EnterpriseUsageMeasure::Storage { bytes } => EnterpriseQuotaAmounts {
            storage_bytes: bytes,
            operations: 1,
            ..EnterpriseQuotaAmounts::default()
        },
        EnterpriseUsageMeasure::Publication => EnterpriseQuotaAmounts {
            operations: 1,
            ..EnterpriseQuotaAmounts::default()
        },
    };
    validate_amounts(amounts)?;
    Ok(amounts)
}

fn amounts_from_totals(
    totals: EnterpriseUsageTotals,
) -> Result<EnterpriseQuotaAmounts, EnterpriseQuotaError> {
    Ok(EnterpriseQuotaAmounts {
        tokens: checked_add(
            totals.provider_total_tokens,
            totals.worker_tokens,
            "settledTokens",
        )?,
        provider_cost_micros: totals.provider_cost_micros,
        worker_cost_microunits: totals.worker_cost_microunits,
        worker_runtime_millis: totals.worker_runtime_millis,
        storage_bytes: totals.storage_bytes,
        operations: totals.entries,
    })
}

fn load_policy(
    connection: &rusqlite::Connection,
    key: &str,
) -> Result<Option<EnterpriseQuotaPolicyReceipt>, EnterpriseQuotaError> {
    connection
        .query_row(
            "SELECT boundary_json, revision, limits_json, policy_digest
             FROM enterprise_quota_policies WHERE boundary_key = ?1",
            [key],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            },
        )
        .optional()
        .map_err(|error| sql_error(&error))?
        .map(|(boundary_json, revision, limits_json, stored_digest)| {
            complete_policy(key, &boundary_json, revision, &limits_json, &stored_digest)
        })
        .transpose()
}

fn complete_policy(
    key: &str,
    boundary_json: &str,
    revision: i64,
    limits_json: &str,
    stored_digest: &str,
) -> Result<EnterpriseQuotaPolicyReceipt, EnterpriseQuotaError> {
    let boundary: EnterpriseQuotaBoundary = serde_json::from_str(boundary_json)
        .map_err(|_| EnterpriseQuotaError::corrupt("stored quota boundary JSON is invalid"))?;
    let limits: EnterpriseQuotaLimits = serde_json::from_str(limits_json)
        .map_err(|_| EnterpriseQuotaError::corrupt("stored quota limits JSON is invalid"))?;
    let revision = u64::try_from(revision)
        .map_err(|_| EnterpriseQuotaError::corrupt("stored quota revision is invalid"))?;
    let policy = EnterpriseQuotaPolicy {
        boundary,
        revision,
        limits,
    };
    validate_policy(&policy)
        .map_err(|_| EnterpriseQuotaError::corrupt("stored quota policy is malformed"))?;
    let policy_digest = digest(&policy)?;
    if encode(&policy.boundary)? != boundary_json
        || encode(&policy.limits)? != limits_json
        || boundary_key(&policy.boundary)? != key
        || policy_digest != stored_digest
    {
        return Err(EnterpriseQuotaError::corrupt(
            "stored quota policy differs from its canonical value",
        ));
    }
    Ok(EnterpriseQuotaPolicyReceipt {
        policy,
        policy_digest,
        idempotent_replay: false,
    })
}

fn validate_policy(policy: &EnterpriseQuotaPolicy) -> Result<(), EnterpriseQuotaError> {
    validate_boundary(&policy.boundary)?;
    positive_safe(policy.revision, "revision")?;
    validate_limits(policy.limits)
}

fn validate_limits(limits: EnterpriseQuotaLimits) -> Result<(), EnterpriseQuotaError> {
    let values = [
        limits.max_concurrent,
        limits.tokens,
        limits.provider_cost_micros,
        limits.worker_cost_microunits,
        limits.worker_runtime_millis,
        limits.storage_bytes,
        limits.operations,
    ];
    if values.iter().all(Option::is_none) {
        return Err(EnterpriseQuotaError::invalid(
            "enterprise quota policy must limit at least one dimension",
        ));
    }
    if values
        .into_iter()
        .flatten()
        .any(|value| value > MAX_SAFE_INTEGER)
    {
        return Err(EnterpriseQuotaError::invalid(
            "enterprise quota limit exceeds the safe integer range",
        ));
    }
    Ok(())
}

fn validate_reservation_request(
    request: &EnterpriseQuotaReservationRequest,
) -> Result<(), EnterpriseQuotaError> {
    validate_id(&request.reservation_id.0, "req_", "reservationId")?;
    crate::enterprise_usage::validate_quota_attribution(&request.attribution)
        .map_err(|error| usage_error(&error))?;
    validate_source_seal(&request.source_seal)?;
    validate_amounts(request.reserved)?;
    if request.reserved.is_zero() || request.reserved.operations != 1 {
        return Err(EnterpriseQuotaError::invalid(
            "enterprise quota reservation must reserve exactly one operation",
        ));
    }
    validate_amount_shape(&request.source_seal, request.reserved)?;
    if matches!(
        request.source_seal,
        EnterpriseQuotaSourceSeal::Provider { .. } | EnterpriseQuotaSourceSeal::Worker { .. }
    ) && request.attribution.product_session_id.is_none()
    {
        return Err(EnterpriseQuotaError::invalid(
            "Provider and Worker quota authority requires a ProductSession",
        ));
    }
    validate_instant(&request.requested_at, "requestedAt")
}

fn validate_source_seal(seal: &EnterpriseQuotaSourceSeal) -> Result<(), EnterpriseQuotaError> {
    match seal {
        EnterpriseQuotaSourceSeal::Provider {
            model_exchange_id,
            request_id,
            attempt,
            route_authority_fingerprint,
        } => {
            validate_id(&model_exchange_id.0, "mdl_", "modelExchangeId")?;
            validate_id(&request_id.0, "req_", "sourceRequestId")?;
            positive_safe(*attempt, "attempt")?;
            validate_input_digest(route_authority_fingerprint, "routeAuthorityFingerprint")
        }
        EnterpriseQuotaSourceSeal::Worker {
            job_id,
            worker_pool_id,
        } => {
            validate_id(&job_id.0, "job_", "jobId")?;
            validate_id(worker_pool_id, "wpl_", "workerPoolId")
        }
        EnterpriseQuotaSourceSeal::Storage {
            artifact_id,
            request_id,
            expected_bytes,
            ..
        } => {
            validate_id(&artifact_id.0, "art_", "artifactId")?;
            validate_id(&request_id.0, "req_", "sourceRequestId")?;
            positive_safe(*expected_bytes, "expectedBytes")
        }
        EnterpriseQuotaSourceSeal::Publication {
            publication_id,
            operation_key,
            request_sha256,
        } => {
            validate_id(&publication_id.0, "pub_", "publicationId")?;
            validate_portable_token(operation_key, "operationKey")?;
            validate_input_digest(request_sha256, "requestSha256")
        }
    }
}

fn validate_amount_shape(
    seal: &EnterpriseQuotaSourceSeal,
    amounts: EnterpriseQuotaAmounts,
) -> Result<(), EnterpriseQuotaError> {
    let valid = match seal {
        EnterpriseQuotaSourceSeal::Provider { .. } => {
            amounts.worker_cost_microunits == 0
                && amounts.worker_runtime_millis == 0
                && amounts.storage_bytes == 0
        }
        EnterpriseQuotaSourceSeal::Worker { .. } => {
            amounts.provider_cost_micros == 0 && amounts.storage_bytes == 0
        }
        EnterpriseQuotaSourceSeal::Storage { expected_bytes, .. } => {
            amounts.tokens == 0
                && amounts.provider_cost_micros == 0
                && amounts.worker_cost_microunits == 0
                && amounts.worker_runtime_millis == 0
                && amounts.storage_bytes == *expected_bytes
        }
        EnterpriseQuotaSourceSeal::Publication { .. } => {
            amounts.tokens == 0
                && amounts.provider_cost_micros == 0
                && amounts.worker_cost_microunits == 0
                && amounts.worker_runtime_millis == 0
                && amounts.storage_bytes == 0
        }
    };
    if valid {
        Ok(())
    } else {
        Err(EnterpriseQuotaError::invalid(
            "enterprise quota quantities do not match the sealed source family",
        ))
    }
}

fn validate_release(release: &EnterpriseQuotaRelease) -> Result<(), EnterpriseQuotaError> {
    validate_id(&release.reservation_id.0, "req_", "reservationId")?;
    validate_id(&release.request_id.0, "req_", "requestId")?;
    positive_safe(release.expected_revision, "expectedRevision")?;
    validate_instant(&release.released_at, "releasedAt")
}

fn validate_settlement(settlement: &EnterpriseQuotaSettlement) -> Result<(), EnterpriseQuotaError> {
    validate_id(&settlement.reservation_id.0, "req_", "reservationId")?;
    validate_id(&settlement.request_id.0, "req_", "requestId")?;
    positive_safe(settlement.expected_revision, "expectedRevision")?;
    Ok(())
}

fn validate_record(record: &EnterpriseQuotaReservationRecord) -> Result<(), EnterpriseQuotaError> {
    validate_reservation_request(&record.request()).map_err(|_| {
        EnterpriseQuotaError::corrupt("stored quota reservation authority is malformed")
    })?;
    positive_safe(record.revision, "revision")?;
    let applicable = applicable_boundaries(&record.attribution)
        .iter()
        .map(boundary_key)
        .collect::<Result<HashSet<_>, _>>()?;
    let mut sealed = HashSet::new();
    for seal in &record.policy_seals {
        validate_boundary(&seal.boundary)?;
        positive_safe(seal.revision, "policyRevision")?;
        validate_digest(&seal.policy_digest, "policyDigest")?;
        let key = boundary_key(&seal.boundary)?;
        if !applicable.contains(&key) || !sealed.insert(key) {
            return Err(EnterpriseQuotaError::corrupt(
                "stored quota policy seals are foreign or duplicated",
            ));
        }
    }
    match (&record.state, &record.terminal) {
        (EnterpriseQuotaReservationState::Active, None) if record.revision == 1 => Ok(()),
        (
            EnterpriseQuotaReservationState::Released,
            Some(EnterpriseQuotaTerminal::Released {
                request_id,
                released_at,
                ..
            }),
        ) if record.revision == 2 => {
            validate_id(&request_id.0, "req_", "terminalRequestId")?;
            validate_instant(released_at, "releasedAt")?;
            require_terminal_order(&record.requested_at, released_at)
        }
        (
            EnterpriseQuotaReservationState::Settled,
            Some(EnterpriseQuotaTerminal::Settled {
                request_id,
                usage_sequence,
                usage_source_digest,
                settled_at,
                ..
            }),
        ) if record.revision == 2 => {
            validate_id(&request_id.0, "req_", "terminalRequestId")?;
            positive_safe(*usage_sequence, "usageSequence")?;
            validate_digest(usage_source_digest, "usageSourceDigest")?;
            validate_instant(settled_at, "settledAt")?;
            require_terminal_order(&record.requested_at, settled_at)
        }
        _ => Err(EnterpriseQuotaError::corrupt(
            "stored quota reservation lifecycle is inconsistent",
        )),
    }
}

fn require_terminal_order(
    requested_at: &Instant,
    terminal_at: &Instant,
) -> Result<(), EnterpriseQuotaError> {
    if terminal_at.0 < requested_at.0 {
        return Err(EnterpriseQuotaError::corrupt(
            "stored quota terminal time precedes reservation",
        ));
    }
    Ok(())
}

fn validate_amounts(amounts: EnterpriseQuotaAmounts) -> Result<(), EnterpriseQuotaError> {
    if [
        amounts.tokens,
        amounts.provider_cost_micros,
        amounts.worker_cost_microunits,
        amounts.worker_runtime_millis,
        amounts.storage_bytes,
        amounts.operations,
    ]
    .into_iter()
    .any(|value| value > MAX_SAFE_INTEGER)
    {
        return Err(EnterpriseQuotaError::invalid(
            "enterprise quota amount exceeds the safe integer range",
        ));
    }
    Ok(())
}

fn validate_boundary(boundary: &EnterpriseQuotaBoundary) -> Result<(), EnterpriseQuotaError> {
    let filter = boundary_filter(boundary);
    for (value, prefix, field) in [
        filter
            .organization_id
            .as_ref()
            .map(|id| (id.0.as_str(), "org_", "organizationId")),
        filter
            .workspace_id
            .as_ref()
            .map(|id| (id.0.as_str(), "wsp_", "workspaceId")),
        filter
            .project_id
            .as_ref()
            .map(|id| (id.0.as_str(), "prj_", "projectId")),
        filter
            .repository_id
            .as_ref()
            .map(|id| (id.0.as_str(), "rep_", "repositoryId")),
        filter
            .delivery_id
            .as_ref()
            .map(|id| (id.0.as_str(), "dlv_", "deliveryId")),
        filter
            .product_session_id
            .as_ref()
            .map(|id| (id.0.as_str(), "psn_", "productSessionId")),
        filter
            .user_id
            .as_ref()
            .map(|id| (id.0.as_str(), "usr_", "userId")),
    ]
    .into_iter()
    .flatten()
    {
        validate_id(value, prefix, field)?;
    }
    Ok(())
}

fn applicable_boundaries(attribution: &EnterpriseUsageAttribution) -> Vec<EnterpriseQuotaBoundary> {
    let mut boundaries = vec![
        EnterpriseQuotaBoundary::Organization {
            organization_id: attribution.organization_id.clone(),
        },
        EnterpriseQuotaBoundary::Workspace {
            organization_id: attribution.organization_id.clone(),
            workspace_id: attribution.workspace_id.clone(),
        },
        EnterpriseQuotaBoundary::Project {
            organization_id: attribution.organization_id.clone(),
            workspace_id: attribution.workspace_id.clone(),
            project_id: attribution.project_id.clone(),
        },
        EnterpriseQuotaBoundary::Repository {
            organization_id: attribution.organization_id.clone(),
            workspace_id: attribution.workspace_id.clone(),
            project_id: attribution.project_id.clone(),
            repository_id: attribution.repository_id.clone(),
        },
    ];
    if let Some(delivery_id) = &attribution.delivery_id {
        boundaries.push(EnterpriseQuotaBoundary::Delivery {
            organization_id: attribution.organization_id.clone(),
            delivery_id: delivery_id.clone(),
        });
    }
    if let Some(product_session_id) = &attribution.product_session_id {
        boundaries.push(EnterpriseQuotaBoundary::ProductSession {
            organization_id: attribution.organization_id.clone(),
            product_session_id: product_session_id.clone(),
        });
    }
    boundaries.push(EnterpriseQuotaBoundary::User {
        organization_id: attribution.organization_id.clone(),
        user_id: attribution.user_id.clone(),
    });
    boundaries
}

fn boundary_filter(boundary: &EnterpriseQuotaBoundary) -> EnterpriseUsageFilter {
    let mut filter = EnterpriseUsageFilter::default();
    match boundary {
        EnterpriseQuotaBoundary::Organization { organization_id } => {
            filter.organization_id = Some(organization_id.clone());
        }
        EnterpriseQuotaBoundary::Workspace {
            organization_id,
            workspace_id,
        } => {
            filter.organization_id = Some(organization_id.clone());
            filter.workspace_id = Some(workspace_id.clone());
        }
        EnterpriseQuotaBoundary::Project {
            organization_id,
            workspace_id,
            project_id,
        } => {
            filter.organization_id = Some(organization_id.clone());
            filter.workspace_id = Some(workspace_id.clone());
            filter.project_id = Some(project_id.clone());
        }
        EnterpriseQuotaBoundary::Repository {
            organization_id,
            workspace_id,
            project_id,
            repository_id,
        } => {
            filter.organization_id = Some(organization_id.clone());
            filter.workspace_id = Some(workspace_id.clone());
            filter.project_id = Some(project_id.clone());
            filter.repository_id = Some(repository_id.clone());
        }
        EnterpriseQuotaBoundary::Delivery {
            organization_id,
            delivery_id,
        } => {
            filter.organization_id = Some(organization_id.clone());
            filter.delivery_id = Some(delivery_id.clone());
        }
        EnterpriseQuotaBoundary::ProductSession {
            organization_id,
            product_session_id,
        } => {
            filter.organization_id = Some(organization_id.clone());
            filter.product_session_id = Some(product_session_id.clone());
        }
        EnterpriseQuotaBoundary::User {
            organization_id,
            user_id,
        } => {
            filter.organization_id = Some(organization_id.clone());
            filter.user_id = Some(user_id.clone());
        }
    }
    filter
}

fn boundary_key(boundary: &EnterpriseQuotaBoundary) -> Result<String, EnterpriseQuotaError> {
    Ok(format!(
        "enterprise-quota:{:x}",
        Sha256::digest(serde_json::to_vec(boundary).map_err(|_| {
            EnterpriseQuotaError::invalid("enterprise quota boundary is not serializable")
        })?)
    ))
}

fn source_seal_key(seal: &EnterpriseQuotaSourceSeal) -> Result<String, EnterpriseQuotaError> {
    Ok(format!(
        "enterprise-quota-source:{:x}",
        Sha256::digest(serde_json::to_vec(seal).map_err(|_| {
            EnterpriseQuotaError::invalid("enterprise quota source seal is not serializable")
        })?)
    ))
}

fn source_seal_from_usage(
    usage: &EnterpriseUsageEntry,
) -> Result<EnterpriseQuotaSourceSeal, EnterpriseQuotaError> {
    match (&usage.fact.source, &usage.fact.measure) {
        (
            EnterpriseUsageSource::Provider {
                model_exchange_id,
                request_id,
                attempt,
                route_authority_fingerprint,
                ..
            },
            crate::EnterpriseUsageMeasure::Provider { .. },
        ) => Ok(EnterpriseQuotaSourceSeal::Provider {
            model_exchange_id: model_exchange_id.clone(),
            request_id: request_id.clone(),
            attempt: *attempt,
            route_authority_fingerprint: route_authority_fingerprint.clone(),
        }),
        (
            EnterpriseUsageSource::Worker {
                job_id,
                worker_pool_id,
                ..
            },
            crate::EnterpriseUsageMeasure::Worker { .. },
        ) => Ok(EnterpriseQuotaSourceSeal::Worker {
            job_id: job_id.clone(),
            worker_pool_id: worker_pool_id.clone(),
        }),
        (
            EnterpriseUsageSource::Storage {
                artifact_id,
                operation_kind,
                request_id,
                ..
            },
            crate::EnterpriseUsageMeasure::Storage { bytes },
        ) => Ok(EnterpriseQuotaSourceSeal::Storage {
            artifact_id: artifact_id.clone(),
            operation_kind: *operation_kind,
            request_id: request_id.clone(),
            expected_bytes: *bytes,
        }),
        (
            EnterpriseUsageSource::Publication {
                publication_id,
                operation_key,
                request_sha256,
            },
            crate::EnterpriseUsageMeasure::Publication,
        ) => Ok(EnterpriseQuotaSourceSeal::Publication {
            publication_id: publication_id.clone(),
            operation_key: operation_key.clone(),
            request_sha256: request_sha256.clone(),
        }),
        _ => Err(EnterpriseQuotaError::corrupt(
            "enterprise quota Usage source and measure families differ",
        )),
    }
}

fn quota_settlement_request_id(source_key: &str) -> RequestId {
    let digest = Sha256::digest(
        [
            b"enterprise-quota:settle:".as_slice(),
            source_key.as_bytes(),
        ]
        .concat(),
    );
    let suffix = digest
        .iter()
        .take(26)
        .map(|byte| char::from(CROCKFORD_BASE32[usize::from(byte & 31)]))
        .collect::<String>();
    RequestId(format!("req_{suffix}"))
}

fn next_revision(revision: u64) -> Result<u64, EnterpriseQuotaError> {
    revision
        .checked_add(1)
        .filter(|value| *value <= MAX_SAFE_INTEGER)
        .ok_or_else(|| EnterpriseQuotaError::invalid("quota revision overflows"))
}

fn checked_add(left: u64, right: u64, field: &str) -> Result<u64, EnterpriseQuotaError> {
    left.checked_add(right)
        .filter(|value| *value <= MAX_SAFE_INTEGER)
        .ok_or_else(|| EnterpriseQuotaError::corrupt(format!("{field} quota total overflows")))
}

fn sql_integer(value: u64) -> Result<i64, EnterpriseQuotaError> {
    if value > MAX_SAFE_INTEGER {
        return Err(EnterpriseQuotaError::invalid(
            "enterprise quota value exceeds the safe integer range",
        ));
    }
    i64::try_from(value)
        .map_err(|_| EnterpriseQuotaError::invalid("enterprise quota value exceeds SQLite range"))
}

fn validate_id(value: &str, prefix: &str, field: &str) -> Result<(), EnterpriseQuotaError> {
    let valid = value.strip_prefix(prefix).is_some_and(|suffix| {
        suffix.len() == 26
            && suffix.bytes().all(|byte| {
                byte.is_ascii_digit()
                    || matches!(
                        byte,
                        b'A'..=b'H' | b'J'..=b'K' | b'M'..=b'N' | b'P'..=b'T' | b'V'..=b'Z'
                    )
            })
    });
    if valid {
        Ok(())
    } else {
        Err(EnterpriseQuotaError::invalid(format!(
            "{field} is not canonical"
        )))
    }
}

fn validate_instant(value: &Instant, field: &str) -> Result<(), EnterpriseQuotaError> {
    let bytes = value.0.as_bytes();
    let valid = bytes.len() == 24
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes[10] == b'T'
        && bytes[13] == b':'
        && bytes[16] == b':'
        && bytes[19] == b'.'
        && bytes[23] == b'Z'
        && bytes.iter().enumerate().all(|(index, byte)| {
            matches!(index, 4 | 7 | 10 | 13 | 16 | 19 | 23) || byte.is_ascii_digit()
        });
    if valid {
        Ok(())
    } else {
        Err(EnterpriseQuotaError::invalid(format!(
            "{field} is not a canonical millisecond UTC instant"
        )))
    }
}

fn validate_digest(value: &str, field: &str) -> Result<(), EnterpriseQuotaError> {
    let valid = value.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    });
    if valid {
        Ok(())
    } else {
        Err(EnterpriseQuotaError::corrupt(format!(
            "{field} is not a canonical SHA-256 digest"
        )))
    }
}

fn validate_input_digest(value: &str, field: &str) -> Result<(), EnterpriseQuotaError> {
    let valid = value.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    });
    if valid {
        Ok(())
    } else {
        Err(EnterpriseQuotaError::invalid(format!(
            "{field} is not a canonical SHA-256 digest"
        )))
    }
}

fn validate_portable_token(value: &str, field: &str) -> Result<(), EnterpriseQuotaError> {
    if !value.is_empty()
        && value.len() <= 512
        && value
            .bytes()
            .all(|byte| byte.is_ascii_graphic() && !matches!(byte, b'\"' | b'\\'))
    {
        Ok(())
    } else {
        Err(EnterpriseQuotaError::invalid(format!(
            "{field} is not a portable token"
        )))
    }
}

fn digest(value: &impl Serialize) -> Result<String, EnterpriseQuotaError> {
    let bytes = serde_json::to_vec(value)
        .map_err(|_| EnterpriseQuotaError::invalid("quota value is not serializable"))?;
    Ok(format!("sha256:{:x}", Sha256::digest(bytes)))
}

fn encode(value: &impl Serialize) -> Result<String, EnterpriseQuotaError> {
    serde_json::to_string(value)
        .map_err(|_| EnterpriseQuotaError::invalid("quota value is not serializable"))
}

fn positive_safe(value: u64, field: &str) -> Result<(), EnterpriseQuotaError> {
    if value == 0 || value > MAX_SAFE_INTEGER {
        return Err(EnterpriseQuotaError::invalid(format!(
            "{field} is outside the safe positive range"
        )));
    }
    Ok(())
}

fn usage_error(error: &EnterpriseUsageError) -> EnterpriseQuotaError {
    match error.kind() {
        EnterpriseUsageErrorKind::CorruptState | EnterpriseUsageErrorKind::SourceConflict => {
            EnterpriseQuotaError::corrupt(format!("enterprise Usage authority failed: {error}"))
        }
        EnterpriseUsageErrorKind::InvalidInput => EnterpriseQuotaError::authority_mismatch(
            "enterprise quota referenced malformed Usage authority",
        ),
        EnterpriseUsageErrorKind::Adapter => {
            EnterpriseQuotaError::adapter(format!("enterprise Usage authority failed: {error}"))
        }
    }
}

fn storage_error(error: &StorageError) -> EnterpriseQuotaError {
    EnterpriseQuotaError::adapter(format!("quota storage adapter failed: {error}"))
}

fn sql_error(error: &rusqlite::Error) -> EnterpriseQuotaError {
    EnterpriseQuotaError::adapter(format!("quota SQLite operation failed: {error}"))
}
