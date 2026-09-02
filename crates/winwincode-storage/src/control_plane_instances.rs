// SPDX-License-Identifier: Apache-2.0

//! Durable multi-instance Control Plane ownership and command fencing.
//!
//! This module stores only operational ownership. Canonical product state and
//! command results remain in [`crate::StateCommit`] and `command_receipts`.
//! A claimed command is committed through [`ControlPlaneInstanceLedger::commit_claimed`]
//! so the instance lease, claim fence, product state, receipt, and outbox share
//! one `SQLite` transaction.

use std::fmt;

use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};
use sha2::{Digest, Sha256};
use winwincode_domain::Sha256Digest;

use crate::{
    CommitReceipt, ReceiptIdentity, SqliteStorage, StateCommit, StorageError,
    commit_claimed_in_transaction,
};

const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
const MAX_ID_BYTES: usize = 96;
const INSTANCE_SCHEMA: &str = r"
CREATE TABLE IF NOT EXISTS control_plane_instances (
    instance_id TEXT PRIMARY KEY NOT NULL,
    boot_id TEXT UNIQUE NOT NULL,
    generation INTEGER NOT NULL CHECK (generation > 0),
    state TEXT NOT NULL CHECK (state IN ('active', 'draining', 'fenced', 'closed')),
    started_at INTEGER NOT NULL CHECK (started_at >= 0),
    renewed_at INTEGER NOT NULL CHECK (renewed_at >= started_at),
    lease_expires_at INTEGER NOT NULL CHECK (lease_expires_at > renewed_at),
    drain_deadline_at INTEGER,
    CHECK (drain_deadline_at IS NULL OR drain_deadline_at >= renewed_at)
);
CREATE TABLE IF NOT EXISTS control_plane_command_claims (
    actor_key BLOB NOT NULL,
    scope_key BLOB NOT NULL,
    request_id TEXT NOT NULL,
    command_digest TEXT NOT NULL,
    instance_id TEXT NOT NULL,
    boot_id TEXT NOT NULL,
    instance_generation INTEGER NOT NULL CHECK (instance_generation > 0),
    claim_fence INTEGER NOT NULL CHECK (claim_fence > 0),
    admitted_at INTEGER NOT NULL CHECK (admitted_at >= 0),
    PRIMARY KEY (actor_key, scope_key, request_id),
    FOREIGN KEY (instance_id) REFERENCES control_plane_instances(instance_id) ON DELETE RESTRICT
);
CREATE INDEX IF NOT EXISTS control_plane_command_claims_by_instance
    ON control_plane_command_claims (instance_id, instance_generation, claim_fence);
";

/// Stable instance identity plus a process-unique boot identity.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ControlPlaneInstanceIdentity {
    instance_id: String,
    boot_id: String,
}

impl ControlPlaneInstanceIdentity {
    /// Builds a canonical instance identity.
    ///
    /// # Errors
    ///
    /// Rejects malformed or overlong instance and boot identifiers.
    pub fn try_new(
        instance_id: impl Into<String>,
        boot_id: impl Into<String>,
    ) -> Result<Self, ControlPlaneInstanceError> {
        let instance_id = instance_id.into();
        let boot_id = boot_id.into();
        validate_id(&instance_id, "cpi_", "Control Plane instance id")?;
        validate_id(&boot_id, "cpb_", "Control Plane boot id")?;
        Ok(Self {
            instance_id,
            boot_id,
        })
    }

    #[must_use]
    pub fn instance_id(&self) -> &str {
        &self.instance_id
    }

    #[must_use]
    pub fn boot_id(&self) -> &str {
        &self.boot_id
    }
}

/// Exact durable lease/fence returned by instance registration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ControlPlaneInstanceAuthority {
    identity: ControlPlaneInstanceIdentity,
    generation: u64,
}

impl ControlPlaneInstanceAuthority {
    #[must_use]
    pub const fn identity(&self) -> &ControlPlaneInstanceIdentity {
        &self.identity
    }

    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }
}

/// Durable lifecycle state of one Control Plane instance.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ControlPlaneInstanceState {
    Active,
    Draining,
    Fenced,
    Closed,
}

impl ControlPlaneInstanceState {
    fn parse(value: &str) -> Result<Self, ControlPlaneInstanceError> {
        match value {
            "active" => Ok(Self::Active),
            "draining" => Ok(Self::Draining),
            "fenced" => Ok(Self::Fenced),
            "closed" => Ok(Self::Closed),
            _ => Err(error(
                ControlPlaneInstanceErrorKind::CorruptState,
                "stored Control Plane instance state is invalid",
            )),
        }
    }
}

/// Secret-free readiness and drain projection from one durable read cut.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ControlPlaneInstanceHealth {
    pub authority: ControlPlaneInstanceAuthority,
    pub state: ControlPlaneInstanceState,
    pub lease_expires_at: u64,
    pub drain_deadline_at: Option<u64>,
    pub lease_valid: bool,
    pub accepting_new_work: bool,
    pub in_flight: u64,
    pub confirmed_state_sequence: u64,
    pub confirmed_state_digest: Sha256Digest,
}

impl ControlPlaneInstanceHealth {
    #[must_use]
    pub fn drained(&self) -> bool {
        self.state == ControlPlaneInstanceState::Draining && self.in_flight == 0
    }
}

/// One exact operational claim for a not-yet-committed canonical command.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ControlPlaneCommandClaim {
    receipt_identity: ReceiptIdentity,
    command_digest: Sha256Digest,
    authority: ControlPlaneInstanceAuthority,
    claim_fence: u64,
    idempotent_replay: bool,
}

impl ControlPlaneCommandClaim {
    #[must_use]
    pub const fn receipt_identity(&self) -> &ReceiptIdentity {
        &self.receipt_identity
    }

    #[must_use]
    pub const fn command_digest(&self) -> &Sha256Digest {
        &self.command_digest
    }

    #[must_use]
    pub const fn authority(&self) -> &ControlPlaneInstanceAuthority {
        &self.authority
    }

    #[must_use]
    pub const fn claim_fence(&self) -> u64 {
        self.claim_fence
    }

    #[must_use]
    pub const fn idempotent_replay(&self) -> bool {
        self.idempotent_replay
    }
}

/// Minimal immutable proof that the unique canonical command receipt exists.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ControlPlaneCommittedCommand {
    pub receipt_identity: ReceiptIdentity,
    pub command_digest: Sha256Digest,
    pub stream_id: String,
    pub revision: u64,
    pub receipt_sequence: u64,
}

/// Result of command admission.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ControlPlaneCommandAdmission {
    Claimed(ControlPlaneCommandClaim),
    Committed(ControlPlaneCommittedCommand),
}

/// Stable operational failure categories.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ControlPlaneInstanceErrorKind {
    InvalidInput,
    LeaseConflict,
    OwnershipLost,
    Draining,
    CommandInFlight,
    CommandFenced,
    RequestConflict,
    ReceiptMissing,
    CorruptState,
    Storage,
}

/// Secret-free instance coordination error.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ControlPlaneInstanceError {
    kind: ControlPlaneInstanceErrorKind,
    message: String,
}

impl ControlPlaneInstanceError {
    #[must_use]
    pub const fn kind(&self) -> ControlPlaneInstanceErrorKind {
        self.kind
    }
}

impl fmt::Display for ControlPlaneInstanceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ControlPlaneInstanceError {}

/// Instance ledger borrowing the sole product-state `SQLite` authority.
pub struct ControlPlaneInstanceLedger<'storage> {
    storage: &'storage mut SqliteStorage,
}

impl SqliteStorage {
    /// Opens durable Control Plane instance coordination on this same product-state database.
    ///
    /// # Errors
    ///
    /// Rejects an unavailable connection or incompatible existing schema.
    pub fn control_plane_instance_ledger(
        &mut self,
    ) -> Result<ControlPlaneInstanceLedger<'_>, ControlPlaneInstanceError> {
        ControlPlaneInstanceLedger::new(self)
    }
}

impl<'storage> ControlPlaneInstanceLedger<'storage> {
    fn new(storage: &'storage mut SqliteStorage) -> Result<Self, ControlPlaneInstanceError> {
        let connection = storage
            .connection()
            .map_err(|storage| storage_error(&storage))?;
        connection
            .execute_batch(INSTANCE_SCHEMA)
            .map_err(|sql| sql_error(&sql))?;
        validate_schema(connection)?;
        Ok(Self { storage })
    }

    /// Registers a fresh boot or replays the exact still-live registration.
    ///
    /// # Errors
    ///
    /// Rejects invalid times, a live conflicting boot, an expired replay, or storage failure.
    pub fn register(
        &mut self,
        identity: &ControlPlaneInstanceIdentity,
        now: u64,
        lease_expires_at: u64,
    ) -> Result<ControlPlaneInstanceAuthority, ControlPlaneInstanceError> {
        validate_time_range(now, lease_expires_at)?;
        let transaction = self.transaction()?;
        let current = load_instance(&transaction, identity.instance_id())?;
        let authority = match current {
            None => {
                let updated = transaction
                    .execute(
                        "INSERT INTO control_plane_instances
                         (instance_id, boot_id, generation, state, started_at, renewed_at,
                          lease_expires_at, drain_deadline_at)
                         VALUES (?1, ?2, 1, 'active', ?3, ?3, ?4, NULL)",
                        params![
                            identity.instance_id(),
                            identity.boot_id(),
                            sql_integer(now)?,
                            sql_integer(lease_expires_at)?,
                        ],
                    )
                    .map_err(|sql| map_registration_sql(&sql))?;
                if updated != 1 {
                    return Err(error(
                        ControlPlaneInstanceErrorKind::LeaseConflict,
                        "Control Plane instance generation changed during registration",
                    ));
                }
                ControlPlaneInstanceAuthority {
                    identity: identity.clone(),
                    generation: 1,
                }
            }
            Some(current)
                if current.boot_id == identity.boot_id()
                    && current.lease_expires_at > now
                    && matches!(
                        current.state,
                        ControlPlaneInstanceState::Active | ControlPlaneInstanceState::Draining
                    ) =>
            {
                ControlPlaneInstanceAuthority {
                    identity: identity.clone(),
                    generation: current.generation,
                }
            }
            Some(current) if current.boot_id == identity.boot_id() => {
                return Err(error(
                    ControlPlaneInstanceErrorKind::OwnershipLost,
                    "expired or closed Control Plane boot cannot be revived",
                ));
            }
            Some(current)
                if current.lease_expires_at <= now
                    || matches!(
                        current.state,
                        ControlPlaneInstanceState::Fenced | ControlPlaneInstanceState::Closed
                    ) =>
            {
                let generation = checked_increment(current.generation, "instance generation")?;
                let updated = transaction
                    .execute(
                        "UPDATE control_plane_instances
                         SET boot_id = ?1, generation = ?2, state = 'active', started_at = ?3,
                             renewed_at = ?3, lease_expires_at = ?4, drain_deadline_at = NULL
                         WHERE instance_id = ?5 AND generation = ?6",
                        params![
                            identity.boot_id(),
                            sql_integer(generation)?,
                            sql_integer(now)?,
                            sql_integer(lease_expires_at)?,
                            identity.instance_id(),
                            sql_integer(current.generation)?,
                        ],
                    )
                    .map_err(|sql| map_registration_sql(&sql))?;
                if updated != 1 {
                    return Err(error(
                        ControlPlaneInstanceErrorKind::LeaseConflict,
                        "Control Plane instance generation changed during takeover",
                    ));
                }
                ControlPlaneInstanceAuthority {
                    identity: identity.clone(),
                    generation,
                }
            }
            Some(_) => {
                return Err(error(
                    ControlPlaneInstanceErrorKind::LeaseConflict,
                    "Control Plane instance id is owned by another live boot",
                ));
            }
        };
        transaction.commit().map_err(|sql| sql_error(&sql))?;
        Ok(authority)
    }

    /// Renews the exact live authority without changing its generation.
    ///
    /// # Errors
    ///
    /// Rejects late renewal, stale ownership, non-increasing expiry, or storage failure.
    pub fn renew(
        &mut self,
        authority: &ControlPlaneInstanceAuthority,
        now: u64,
        lease_expires_at: u64,
    ) -> Result<ControlPlaneInstanceHealth, ControlPlaneInstanceError> {
        validate_time_range(now, lease_expires_at)?;
        let transaction = self.transaction()?;
        let current = require_authority(&transaction, authority)?;
        require_live(&current, now)?;
        if lease_expires_at == current.lease_expires_at {
            let health = load_health(&transaction, authority, now)?;
            transaction.commit().map_err(|sql| sql_error(&sql))?;
            return Ok(health);
        }
        if lease_expires_at < current.lease_expires_at {
            return Err(error(
                ControlPlaneInstanceErrorKind::InvalidInput,
                "renewed lease expiry must increase",
            ));
        }
        transaction
            .execute(
                "UPDATE control_plane_instances
                 SET renewed_at = ?1, lease_expires_at = ?2
                 WHERE instance_id = ?3 AND boot_id = ?4 AND generation = ?5",
                params![
                    sql_integer(now)?,
                    sql_integer(lease_expires_at)?,
                    authority.identity().instance_id(),
                    authority.identity().boot_id(),
                    sql_integer(authority.generation())?,
                ],
            )
            .map_err(|sql| sql_error(&sql))?;
        let health = load_health(&transaction, authority, now)?;
        transaction.commit().map_err(|sql| sql_error(&sql))?;
        Ok(health)
    }

    /// Returns one durable readiness/drain snapshot.
    ///
    /// # Errors
    ///
    /// Rejects stale ownership, corrupt rows, or storage failure.
    pub fn preflight(
        &self,
        authority: &ControlPlaneInstanceAuthority,
        now: u64,
    ) -> Result<ControlPlaneInstanceHealth, ControlPlaneInstanceError> {
        validate_time(now, "preflight time")?;
        load_health(
            self.storage
                .connection()
                .map_err(|storage| storage_error(&storage))?,
            authority,
            now,
        )
    }

    /// Atomically stops new command claims while allowing existing claims to finish.
    ///
    /// # Errors
    ///
    /// Rejects expired/stale ownership, invalid deadline, closed state, or storage failure.
    pub fn request_drain(
        &mut self,
        authority: &ControlPlaneInstanceAuthority,
        now: u64,
        drain_deadline_at: u64,
    ) -> Result<ControlPlaneInstanceHealth, ControlPlaneInstanceError> {
        validate_time_range(now, drain_deadline_at)?;
        let transaction = self.transaction()?;
        let current = require_authority(&transaction, authority)?;
        require_live(&current, now)?;
        match current.state {
            ControlPlaneInstanceState::Active => {
                transaction
                    .execute(
                        "UPDATE control_plane_instances SET state = 'draining', drain_deadline_at = ?1
                         WHERE instance_id = ?2 AND boot_id = ?3 AND generation = ?4",
                        params![
                            sql_integer(drain_deadline_at)?,
                            authority.identity().instance_id(),
                            authority.identity().boot_id(),
                            sql_integer(authority.generation())?,
                        ],
                    )
                    .map_err(|sql| sql_error(&sql))?;
            }
            ControlPlaneInstanceState::Draining => {}
            ControlPlaneInstanceState::Fenced | ControlPlaneInstanceState::Closed => {
                return Err(error(
                    ControlPlaneInstanceErrorKind::OwnershipLost,
                    "Control Plane instance is no longer drainable",
                ));
            }
        }
        let health = load_health(&transaction, authority, now)?;
        transaction.commit().map_err(|sql| sql_error(&sql))?;
        Ok(health)
    }

    /// Cancels an in-progress drain before the lease expires.
    ///
    /// # Errors
    ///
    /// Rejects stale/expired ownership, non-draining state, or storage failure.
    pub fn resume(
        &mut self,
        authority: &ControlPlaneInstanceAuthority,
        now: u64,
    ) -> Result<ControlPlaneInstanceHealth, ControlPlaneInstanceError> {
        validate_time(now, "resume time")?;
        let transaction = self.transaction()?;
        let current = require_authority(&transaction, authority)?;
        require_live(&current, now)?;
        if current.state == ControlPlaneInstanceState::Active {
            let health = load_health(&transaction, authority, now)?;
            transaction.commit().map_err(|sql| sql_error(&sql))?;
            return Ok(health);
        }
        if current.state != ControlPlaneInstanceState::Draining {
            return Err(error(
                ControlPlaneInstanceErrorKind::LeaseConflict,
                "only a draining Control Plane instance can resume",
            ));
        }
        transaction
            .execute(
                "UPDATE control_plane_instances SET state = 'active', drain_deadline_at = NULL
                 WHERE instance_id = ?1 AND boot_id = ?2 AND generation = ?3",
                params![
                    authority.identity().instance_id(),
                    authority.identity().boot_id(),
                    sql_integer(authority.generation())?,
                ],
            )
            .map_err(|sql| sql_error(&sql))?;
        let health = load_health(&transaction, authority, now)?;
        transaction.commit().map_err(|sql| sql_error(&sql))?;
        Ok(health)
    }

    /// Closes a fully drained authority. Closed generations cannot resume.
    ///
    /// # Errors
    ///
    /// Rejects active claims, stale/expired ownership, wrong state, or storage failure.
    pub fn release(
        &mut self,
        authority: &ControlPlaneInstanceAuthority,
        now: u64,
    ) -> Result<ControlPlaneInstanceHealth, ControlPlaneInstanceError> {
        validate_time(now, "release time")?;
        let transaction = self.transaction()?;
        let current = require_authority(&transaction, authority)?;
        if current.state == ControlPlaneInstanceState::Closed {
            let health = load_health(&transaction, authority, now)?;
            transaction.commit().map_err(|sql| sql_error(&sql))?;
            return Ok(health);
        }
        require_live(&current, now)?;
        if current.state != ControlPlaneInstanceState::Draining {
            return Err(error(
                ControlPlaneInstanceErrorKind::LeaseConflict,
                "Control Plane instance must drain before release",
            ));
        }
        if count_claims(&transaction, authority)? != 0 {
            return Err(error(
                ControlPlaneInstanceErrorKind::CommandInFlight,
                "Control Plane instance still owns in-flight commands",
            ));
        }
        transaction
            .execute(
                "UPDATE control_plane_instances SET state = 'closed'
                 WHERE instance_id = ?1 AND boot_id = ?2 AND generation = ?3",
                params![
                    authority.identity().instance_id(),
                    authority.identity().boot_id(),
                    sql_integer(authority.generation())?,
                ],
            )
            .map_err(|sql| sql_error(&sql))?;
        let health = load_health(&transaction, authority, now)?;
        transaction.commit().map_err(|sql| sql_error(&sql))?;
        Ok(health)
    }

    /// Fences one expired instance generation so it cannot finish stale work.
    ///
    /// # Errors
    ///
    /// Rejects a live lease, invalid id, missing instance, or storage failure.
    pub fn fence_expired(
        &mut self,
        instance_id: &str,
        now: u64,
    ) -> Result<ControlPlaneInstanceHealth, ControlPlaneInstanceError> {
        validate_id(instance_id, "cpi_", "Control Plane instance id")?;
        validate_time(now, "fence time")?;
        let transaction = self.transaction()?;
        let current = load_instance(&transaction, instance_id)?.ok_or_else(|| {
            error(
                ControlPlaneInstanceErrorKind::OwnershipLost,
                "Control Plane instance does not exist",
            )
        })?;
        if current.lease_expires_at > now
            && matches!(
                current.state,
                ControlPlaneInstanceState::Active | ControlPlaneInstanceState::Draining
            )
        {
            return Err(error(
                ControlPlaneInstanceErrorKind::LeaseConflict,
                "live Control Plane instance cannot be fenced",
            ));
        }
        if current.state != ControlPlaneInstanceState::Closed {
            transaction
                .execute(
                    "UPDATE control_plane_instances SET state = 'fenced'
                     WHERE instance_id = ?1 AND generation = ?2",
                    params![instance_id, sql_integer(current.generation)?],
                )
                .map_err(|sql| sql_error(&sql))?;
        }
        let authority = ControlPlaneInstanceAuthority {
            identity: ControlPlaneInstanceIdentity {
                instance_id: instance_id.to_owned(),
                boot_id: current.boot_id,
            },
            generation: current.generation,
        };
        let health = load_health(&transaction, &authority, now)?;
        transaction.commit().map_err(|sql| sql_error(&sql))?;
        Ok(health)
    }

    /// Claims one not-yet-committed command or reports its immutable receipt.
    ///
    /// Receipt lookup happens before claim arbitration. Therefore a failover
    /// after commit but before response always returns the canonical committed
    /// result and never re-executes business work.
    ///
    /// # Errors
    ///
    /// Rejects draining/expired ownership, changed request reuse, a live
    /// foreign claim, corrupt durable rows, or storage failure.
    pub fn admit_command(
        &mut self,
        authority: &ControlPlaneInstanceAuthority,
        now: u64,
        receipt_identity: &ReceiptIdentity,
        command_digest: &Sha256Digest,
    ) -> Result<ControlPlaneCommandAdmission, ControlPlaneInstanceError> {
        validate_time(now, "command admission time")?;
        validate_digest(command_digest)?;
        let transaction = self.transaction()?;
        let current = require_authority(&transaction, authority)?;
        require_accepting(&current, now)?;
        if let Some(committed) = load_committed(&transaction, receipt_identity)? {
            ensure_digest(&committed.command_digest, command_digest)?;
            transaction.commit().map_err(|sql| sql_error(&sql))?;
            return Ok(ControlPlaneCommandAdmission::Committed(committed));
        }
        let existing = load_claim(&transaction, receipt_identity)?;
        let claim = match existing {
            None => insert_claim(
                &transaction,
                authority,
                now,
                receipt_identity,
                command_digest,
            )?,
            Some(existing) => {
                ensure_digest(&existing.command_digest, command_digest)?;
                if existing.matches_authority(authority) {
                    existing.into_public(receipt_identity.clone(), true)?
                } else {
                    let owner =
                        load_instance(&transaction, &existing.instance_id)?.ok_or_else(|| {
                            error(
                                ControlPlaneInstanceErrorKind::CorruptState,
                                "command claim references a missing Control Plane instance",
                            )
                        })?;
                    let owner_is_live = owner.boot_id == existing.boot_id
                        && owner.generation == existing.instance_generation
                        && owner.lease_expires_at > now
                        && matches!(
                            owner.state,
                            ControlPlaneInstanceState::Active | ControlPlaneInstanceState::Draining
                        );
                    if owner_is_live {
                        return Err(error(
                            ControlPlaneInstanceErrorKind::CommandInFlight,
                            "canonical command is in flight on another live instance",
                        ));
                    }
                    takeover_claim(&transaction, authority, now, receipt_identity, &existing)?
                }
            }
        };
        transaction.commit().map_err(|sql| sql_error(&sql))?;
        Ok(ControlPlaneCommandAdmission::Claimed(claim))
    }

    /// Atomically validates the instance and command fence, writes the sole
    /// canonical [`StateCommit`], and releases the operational claim.
    ///
    /// # Errors
    ///
    /// Rejects expired, drained-to-closed, replaced, changed, or foreign
    /// authority before any product-state write. Storage validation and
    /// revision errors preserve both the claim and original product state.
    pub fn commit_claimed(
        &mut self,
        claim: &ControlPlaneCommandClaim,
        now: u64,
        commit: &StateCommit,
    ) -> Result<CommitReceipt, ControlPlaneInstanceError> {
        validate_time(now, "command commit time")?;
        commit
            .validate()
            .map_err(|storage| storage_error(&storage))?;
        if &commit.receipt_identity != claim.receipt_identity()
            || &commit.command_digest != claim.command_digest()
        {
            return Err(error(
                ControlPlaneInstanceErrorKind::RequestConflict,
                "claimed command differs from its canonical StateCommit",
            ));
        }
        let transaction = self.transaction()?;
        let current = require_authority(&transaction, claim.authority())?;
        require_live(&current, now)?;
        let stored = load_claim(&transaction, claim.receipt_identity())?.ok_or_else(|| {
            error(
                ControlPlaneInstanceErrorKind::CommandFenced,
                "command claim is no longer current",
            )
        })?;
        if !stored.matches_claim(claim) {
            return Err(error(
                ControlPlaneInstanceErrorKind::CommandFenced,
                "command claim was replaced by a newer fence",
            ));
        }
        let receipt = commit_claimed_in_transaction(&transaction, commit)
            .map_err(|storage| storage_error(&storage))?;
        let deleted = transaction
            .execute(
                "DELETE FROM control_plane_command_claims
                 WHERE actor_key = ?1 AND scope_key = ?2 AND request_id = ?3
                   AND instance_id = ?4 AND boot_id = ?5
                   AND instance_generation = ?6 AND claim_fence = ?7",
                params![
                    claim.receipt_identity().actor_key().as_bytes(),
                    claim.receipt_identity().scope_key().as_bytes(),
                    claim.receipt_identity().request_id().0,
                    claim.authority().identity().instance_id(),
                    claim.authority().identity().boot_id(),
                    sql_integer(claim.authority().generation())?,
                    sql_integer(claim.claim_fence())?,
                ],
            )
            .map_err(|sql| sql_error(&sql))?;
        if deleted != 1 {
            return Err(error(
                ControlPlaneInstanceErrorKind::CommandFenced,
                "command claim changed before commit",
            ));
        }
        transaction.commit().map_err(|sql| sql_error(&sql))?;
        Ok(receipt)
    }

    fn transaction(&mut self) -> Result<Transaction<'_>, ControlPlaneInstanceError> {
        self.storage
            .connection_mut()
            .map_err(|storage| storage_error(&storage))?
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|sql| sql_error(&sql))
    }
}

#[derive(Debug)]
struct StoredInstance {
    boot_id: String,
    generation: u64,
    state: ControlPlaneInstanceState,
    lease_expires_at: u64,
    drain_deadline_at: Option<u64>,
}

#[derive(Debug)]
struct StoredClaim {
    command_digest: Sha256Digest,
    instance_id: String,
    boot_id: String,
    instance_generation: u64,
    claim_fence: u64,
}

impl StoredClaim {
    fn matches_authority(&self, authority: &ControlPlaneInstanceAuthority) -> bool {
        self.instance_id == authority.identity().instance_id()
            && self.boot_id == authority.identity().boot_id()
            && self.instance_generation == authority.generation()
    }

    fn matches_claim(&self, claim: &ControlPlaneCommandClaim) -> bool {
        self.matches_authority(claim.authority())
            && self.claim_fence == claim.claim_fence()
            && self.command_digest == *claim.command_digest()
    }

    fn into_public(
        self,
        receipt_identity: ReceiptIdentity,
        idempotent_replay: bool,
    ) -> Result<ControlPlaneCommandClaim, ControlPlaneInstanceError> {
        let identity = ControlPlaneInstanceIdentity::try_new(self.instance_id, self.boot_id)?;
        Ok(ControlPlaneCommandClaim {
            receipt_identity,
            command_digest: self.command_digest,
            authority: ControlPlaneInstanceAuthority {
                identity,
                generation: self.instance_generation,
            },
            claim_fence: self.claim_fence,
            idempotent_replay,
        })
    }
}

fn load_instance(
    connection: &rusqlite::Connection,
    instance_id: &str,
) -> Result<Option<StoredInstance>, ControlPlaneInstanceError> {
    connection
        .query_row(
            "SELECT boot_id, generation, state, lease_expires_at, drain_deadline_at
             FROM control_plane_instances WHERE instance_id = ?1",
            [instance_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, Option<i64>>(4)?,
                ))
            },
        )
        .optional()
        .map_err(|sql| sql_error(&sql))?
        .map(
            |(boot_id, generation, state, lease_expires_at, drain_deadline_at)| {
                Ok(StoredInstance {
                    boot_id,
                    generation: from_sql_integer(generation, "instance generation")?,
                    state: ControlPlaneInstanceState::parse(&state)?,
                    lease_expires_at: from_sql_integer(lease_expires_at, "instance lease expiry")?,
                    drain_deadline_at: drain_deadline_at
                        .map(|value| from_sql_integer(value, "drain deadline"))
                        .transpose()?,
                })
            },
        )
        .transpose()
}

fn require_authority(
    connection: &rusqlite::Connection,
    authority: &ControlPlaneInstanceAuthority,
) -> Result<StoredInstance, ControlPlaneInstanceError> {
    let current =
        load_instance(connection, authority.identity().instance_id())?.ok_or_else(|| {
            error(
                ControlPlaneInstanceErrorKind::OwnershipLost,
                "Control Plane instance authority does not exist",
            )
        })?;
    if current.boot_id != authority.identity().boot_id()
        || current.generation != authority.generation()
    {
        return Err(error(
            ControlPlaneInstanceErrorKind::OwnershipLost,
            "Control Plane instance authority was replaced",
        ));
    }
    Ok(current)
}

fn require_live(current: &StoredInstance, now: u64) -> Result<(), ControlPlaneInstanceError> {
    if current.lease_expires_at <= now
        || matches!(
            current.state,
            ControlPlaneInstanceState::Fenced | ControlPlaneInstanceState::Closed
        )
    {
        return Err(error(
            ControlPlaneInstanceErrorKind::OwnershipLost,
            "Control Plane instance lease is no longer live",
        ));
    }
    Ok(())
}

fn require_accepting(current: &StoredInstance, now: u64) -> Result<(), ControlPlaneInstanceError> {
    require_live(current, now)?;
    if current.state == ControlPlaneInstanceState::Draining {
        return Err(error(
            ControlPlaneInstanceErrorKind::Draining,
            "Control Plane instance is draining",
        ));
    }
    Ok(())
}

fn load_health(
    connection: &rusqlite::Connection,
    authority: &ControlPlaneInstanceAuthority,
    now: u64,
) -> Result<ControlPlaneInstanceHealth, ControlPlaneInstanceError> {
    let current = require_authority(connection, authority)?;
    let in_flight = count_claims(connection, authority)?;
    let confirmed_state_sequence = connection
        .query_row(
            "SELECT COALESCE(MAX(rowid), 0) FROM command_receipts",
            [],
            |row| row.get::<_, i64>(0),
        )
        .map_err(|sql| sql_error(&sql))?;
    let confirmed_state_sequence =
        from_sql_integer(confirmed_state_sequence, "confirmed state sequence")?;
    let confirmed_state_digest = confirmed_state_digest(confirmed_state_sequence);
    let lease_valid = current.lease_expires_at > now
        && !matches!(
            current.state,
            ControlPlaneInstanceState::Fenced | ControlPlaneInstanceState::Closed
        );
    Ok(ControlPlaneInstanceHealth {
        authority: authority.clone(),
        state: current.state,
        lease_expires_at: current.lease_expires_at,
        drain_deadline_at: current.drain_deadline_at,
        lease_valid,
        accepting_new_work: lease_valid && current.state == ControlPlaneInstanceState::Active,
        in_flight,
        confirmed_state_sequence,
        confirmed_state_digest,
    })
}

fn count_claims(
    connection: &rusqlite::Connection,
    authority: &ControlPlaneInstanceAuthority,
) -> Result<u64, ControlPlaneInstanceError> {
    let count = connection
        .query_row(
            "SELECT COUNT(*) FROM control_plane_command_claims
             WHERE instance_id = ?1 AND boot_id = ?2 AND instance_generation = ?3",
            params![
                authority.identity().instance_id(),
                authority.identity().boot_id(),
                sql_integer(authority.generation())?,
            ],
            |row| row.get::<_, i64>(0),
        )
        .map_err(|sql| sql_error(&sql))?;
    from_sql_integer(count, "in-flight command count")
}

fn load_committed(
    connection: &rusqlite::Connection,
    identity: &ReceiptIdentity,
) -> Result<Option<ControlPlaneCommittedCommand>, ControlPlaneInstanceError> {
    connection
        .query_row(
            "SELECT rowid, command_digest, stream_id, revision FROM command_receipts
             WHERE actor_key = ?1 AND scope_key = ?2 AND request_id = ?3",
            params![
                identity.actor_key().as_bytes(),
                identity.scope_key().as_bytes(),
                identity.request_id().0,
            ],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            },
        )
        .optional()
        .map_err(|sql| sql_error(&sql))?
        .map(|(sequence, digest, stream_id, revision)| {
            validate_digest_text(&digest)?;
            Ok(ControlPlaneCommittedCommand {
                receipt_identity: identity.clone(),
                command_digest: Sha256Digest(digest),
                stream_id,
                revision: from_sql_integer(revision, "command receipt revision")?,
                receipt_sequence: from_sql_integer(sequence, "command receipt sequence")?,
            })
        })
        .transpose()
}

fn load_claim(
    connection: &rusqlite::Connection,
    identity: &ReceiptIdentity,
) -> Result<Option<StoredClaim>, ControlPlaneInstanceError> {
    connection
        .query_row(
            "SELECT command_digest, instance_id, boot_id, instance_generation, claim_fence
             FROM control_plane_command_claims
             WHERE actor_key = ?1 AND scope_key = ?2 AND request_id = ?3",
            params![
                identity.actor_key().as_bytes(),
                identity.scope_key().as_bytes(),
                identity.request_id().0,
            ],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                ))
            },
        )
        .optional()
        .map_err(|sql| sql_error(&sql))?
        .map(
            |(command_digest, instance_id, boot_id, instance_generation, claim_fence)| {
                validate_digest_text(&command_digest)?;
                ControlPlaneInstanceIdentity::try_new(instance_id.clone(), boot_id.clone())?;
                Ok(StoredClaim {
                    command_digest: Sha256Digest(command_digest),
                    instance_id,
                    boot_id,
                    instance_generation: from_sql_integer(
                        instance_generation,
                        "claim instance generation",
                    )?,
                    claim_fence: from_sql_integer(claim_fence, "command claim fence")?,
                })
            },
        )
        .transpose()
}

fn insert_claim(
    transaction: &Transaction<'_>,
    authority: &ControlPlaneInstanceAuthority,
    now: u64,
    identity: &ReceiptIdentity,
    command_digest: &Sha256Digest,
) -> Result<ControlPlaneCommandClaim, ControlPlaneInstanceError> {
    transaction
        .execute(
            "INSERT INTO control_plane_command_claims
             (actor_key, scope_key, request_id, command_digest, instance_id, boot_id,
              instance_generation, claim_fence, admitted_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 1, ?8)",
            params![
                identity.actor_key().as_bytes(),
                identity.scope_key().as_bytes(),
                identity.request_id().0,
                command_digest.0,
                authority.identity().instance_id(),
                authority.identity().boot_id(),
                sql_integer(authority.generation())?,
                sql_integer(now)?,
            ],
        )
        .map_err(|sql| sql_error(&sql))?;
    Ok(ControlPlaneCommandClaim {
        receipt_identity: identity.clone(),
        command_digest: command_digest.clone(),
        authority: authority.clone(),
        claim_fence: 1,
        idempotent_replay: false,
    })
}

fn takeover_claim(
    transaction: &Transaction<'_>,
    authority: &ControlPlaneInstanceAuthority,
    now: u64,
    identity: &ReceiptIdentity,
    existing: &StoredClaim,
) -> Result<ControlPlaneCommandClaim, ControlPlaneInstanceError> {
    let claim_fence = checked_increment(existing.claim_fence, "command claim fence")?;
    let updated = transaction
        .execute(
            "UPDATE control_plane_command_claims
             SET instance_id = ?1, boot_id = ?2, instance_generation = ?3,
                 claim_fence = ?4, admitted_at = ?5
             WHERE actor_key = ?6 AND scope_key = ?7 AND request_id = ?8
               AND instance_id = ?9 AND boot_id = ?10
               AND instance_generation = ?11 AND claim_fence = ?12",
            params![
                authority.identity().instance_id(),
                authority.identity().boot_id(),
                sql_integer(authority.generation())?,
                sql_integer(claim_fence)?,
                sql_integer(now)?,
                identity.actor_key().as_bytes(),
                identity.scope_key().as_bytes(),
                identity.request_id().0,
                existing.instance_id,
                existing.boot_id,
                sql_integer(existing.instance_generation)?,
                sql_integer(existing.claim_fence)?,
            ],
        )
        .map_err(|sql| sql_error(&sql))?;
    if updated != 1 {
        return Err(error(
            ControlPlaneInstanceErrorKind::CommandInFlight,
            "command claim changed during takeover",
        ));
    }
    Ok(ControlPlaneCommandClaim {
        receipt_identity: identity.clone(),
        command_digest: existing.command_digest.clone(),
        authority: authority.clone(),
        claim_fence,
        idempotent_replay: false,
    })
}

fn ensure_digest(
    stored: &Sha256Digest,
    supplied: &Sha256Digest,
) -> Result<(), ControlPlaneInstanceError> {
    if stored != supplied {
        return Err(error(
            ControlPlaneInstanceErrorKind::RequestConflict,
            "request identity was already used with another command digest",
        ));
    }
    Ok(())
}

fn confirmed_state_digest(sequence: u64) -> Sha256Digest {
    let mut digest = Sha256::new();
    digest.update(b"winwincode.control-plane-confirmed-state.v1\0");
    digest.update(sequence.to_be_bytes());
    Sha256Digest(format!("sha256:{:x}", digest.finalize()))
}

fn validate_schema(connection: &rusqlite::Connection) -> Result<(), ControlPlaneInstanceError> {
    validate_columns(
        connection,
        "control_plane_instances",
        &[
            "instance_id",
            "boot_id",
            "generation",
            "state",
            "started_at",
            "renewed_at",
            "lease_expires_at",
            "drain_deadline_at",
        ],
    )?;
    validate_columns(
        connection,
        "control_plane_command_claims",
        &[
            "actor_key",
            "scope_key",
            "request_id",
            "command_digest",
            "instance_id",
            "boot_id",
            "instance_generation",
            "claim_fence",
            "admitted_at",
        ],
    )
}

fn validate_columns(
    connection: &rusqlite::Connection,
    table: &str,
    expected: &[&str],
) -> Result<(), ControlPlaneInstanceError> {
    let pragma = format!("PRAGMA table_info({table})");
    let mut statement = connection.prepare(&pragma).map_err(|sql| sql_error(&sql))?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|sql| sql_error(&sql))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|sql| sql_error(&sql))?;
    if columns != expected {
        return Err(error(
            ControlPlaneInstanceErrorKind::CorruptState,
            "Control Plane instance schema is incompatible",
        ));
    }
    Ok(())
}

fn validate_id(value: &str, prefix: &str, label: &str) -> Result<(), ControlPlaneInstanceError> {
    if value.len() <= prefix.len()
        || value.len() > MAX_ID_BYTES
        || !value.starts_with(prefix)
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return Err(error(
            ControlPlaneInstanceErrorKind::InvalidInput,
            format!("{label} is invalid"),
        ));
    }
    Ok(())
}

fn validate_digest(digest: &Sha256Digest) -> Result<(), ControlPlaneInstanceError> {
    if canonical_digest(&digest.0) {
        return Ok(());
    }
    Err(error(
        ControlPlaneInstanceErrorKind::InvalidInput,
        "command digest is not canonical SHA-256",
    ))
}

fn validate_digest_text(value: &str) -> Result<(), ControlPlaneInstanceError> {
    if canonical_digest(value) {
        return Ok(());
    }
    Err(error(
        ControlPlaneInstanceErrorKind::CorruptState,
        "stored command digest is not canonical SHA-256",
    ))
}

fn canonical_digest(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    })
}

fn validate_time(value: u64, label: &str) -> Result<(), ControlPlaneInstanceError> {
    if value > MAX_SAFE_INTEGER {
        return Err(error(
            ControlPlaneInstanceErrorKind::InvalidInput,
            format!("{label} exceeds the safe integer range"),
        ));
    }
    Ok(())
}

fn validate_time_range(start: u64, end: u64) -> Result<(), ControlPlaneInstanceError> {
    validate_time(start, "lease time")?;
    validate_time(end, "lease expiry")?;
    if end <= start {
        return Err(error(
            ControlPlaneInstanceErrorKind::InvalidInput,
            "lease or deadline must be later than the current time",
        ));
    }
    Ok(())
}

fn checked_increment(value: u64, label: &str) -> Result<u64, ControlPlaneInstanceError> {
    let value = value.checked_add(1).ok_or_else(|| {
        error(
            ControlPlaneInstanceErrorKind::CorruptState,
            format!("{label} overflowed"),
        )
    })?;
    if value > MAX_SAFE_INTEGER {
        return Err(error(
            ControlPlaneInstanceErrorKind::CorruptState,
            format!("{label} exceeds the safe integer range"),
        ));
    }
    Ok(value)
}

fn sql_integer(value: u64) -> Result<i64, ControlPlaneInstanceError> {
    i64::try_from(value).map_err(|_| {
        error(
            ControlPlaneInstanceErrorKind::InvalidInput,
            "numeric value exceeds the SQLite integer range",
        )
    })
}

fn from_sql_integer(value: i64, label: &str) -> Result<u64, ControlPlaneInstanceError> {
    let value = u64::try_from(value).map_err(|_| {
        error(
            ControlPlaneInstanceErrorKind::CorruptState,
            format!("stored {label} is negative"),
        )
    })?;
    if value > MAX_SAFE_INTEGER {
        return Err(error(
            ControlPlaneInstanceErrorKind::CorruptState,
            format!("stored {label} exceeds the safe integer range"),
        ));
    }
    Ok(value)
}

fn map_registration_sql(sql: &rusqlite::Error) -> ControlPlaneInstanceError {
    if matches!(
        sql,
        rusqlite::Error::SqliteFailure(code, _)
            if code.code == rusqlite::ErrorCode::ConstraintViolation
    ) {
        return error(
            ControlPlaneInstanceErrorKind::LeaseConflict,
            "Control Plane instance registration conflicts with durable ownership",
        );
    }
    sql_error(sql)
}

fn storage_error(storage: &StorageError) -> ControlPlaneInstanceError {
    error(
        ControlPlaneInstanceErrorKind::Storage,
        format!("Control Plane instance storage failed: {storage}"),
    )
}

fn sql_error(_sql: &rusqlite::Error) -> ControlPlaneInstanceError {
    error(
        ControlPlaneInstanceErrorKind::Storage,
        "Control Plane instance storage operation failed",
    )
}

fn error(
    kind: ControlPlaneInstanceErrorKind,
    message: impl Into<String>,
) -> ControlPlaneInstanceError {
    ControlPlaneInstanceError {
        kind,
        message: message.into(),
    }
}
