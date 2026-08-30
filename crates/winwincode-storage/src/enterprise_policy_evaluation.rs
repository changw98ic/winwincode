// SPDX-License-Identifier: Apache-2.0

//! Deterministic enterprise Policy evaluation, exception seals, and audit receipts.
//!
//! Policy definitions remain owned by [`crate::EnterprisePolicyLedger`]. This
//! module consumes one immutable effective version inside the same `SQLite`
//! transaction that records an enforced evaluation or an exception mutation.
//! Dry runs execute the identical evaluator in a read-only transaction.

use std::collections::HashSet;
use std::fmt;

use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use winwincode_domain::{Instant, RequestId, Sha256Digest};

use crate::enterprise_policy::resolve_effective_policy;
use crate::{
    EnterprisePolicyActor, EnterprisePolicyEffect, EnterprisePolicyError,
    EnterprisePolicyErrorKind, EnterprisePolicyKind, EnterprisePolicyMode, EnterprisePolicyRule,
    EnterprisePolicyScope, EnterprisePolicyVersion, EnterprisePolicyVersionReference,
    SqliteStorage, StorageError,
};

const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
const MAX_PAGE_SIZE: u64 = 200;
const MAX_RESOURCE_BYTES: usize = 2_048;
const MAX_CONDITIONS: usize = 256;
const EVALUATION_SCHEMA: &str = r"
CREATE TABLE IF NOT EXISTS enterprise_policy_exception_versions (
    sequence INTEGER PRIMARY KEY AUTOINCREMENT CHECK (sequence > 0),
    exception_id TEXT NOT NULL,
    version INTEGER NOT NULL CHECK (version > 0),
    revision INTEGER NOT NULL CHECK (revision > 0),
    scope_digest TEXT NOT NULL,
    policy_kind TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('pending', 'approved', 'rejected', 'escalated')),
    expires_at TEXT NOT NULL,
    record_digest TEXT UNIQUE NOT NULL,
    record_json TEXT NOT NULL,
    UNIQUE (exception_id, version),
    UNIQUE (exception_id, revision)
);
CREATE TABLE IF NOT EXISTS enterprise_policy_exception_heads (
    exception_id TEXT PRIMARY KEY NOT NULL,
    version INTEGER NOT NULL CHECK (version > 0),
    revision INTEGER NOT NULL CHECK (revision > 0),
    record_digest TEXT UNIQUE NOT NULL,
    FOREIGN KEY (exception_id, version)
        REFERENCES enterprise_policy_exception_versions(exception_id, version) ON DELETE RESTRICT
);
CREATE TABLE IF NOT EXISTS enterprise_policy_evaluation_audit (
    sequence INTEGER PRIMARY KEY AUTOINCREMENT CHECK (sequence > 0),
    scope_digest TEXT NOT NULL,
    policy_kind TEXT NOT NULL,
    evaluated_at TEXT NOT NULL,
    input_digest TEXT NOT NULL,
    decision_digest TEXT UNIQUE NOT NULL,
    record_json TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS enterprise_policy_evaluation_receipts (
    actor_digest TEXT NOT NULL,
    scope_digest TEXT NOT NULL,
    request_id TEXT NOT NULL,
    command_kind TEXT NOT NULL CHECK (command_kind IN ('evaluate', 'exception_request', 'exception_decide')),
    command_digest TEXT NOT NULL,
    record_kind TEXT NOT NULL CHECK (record_kind IN ('audit', 'exception')),
    record_id TEXT NOT NULL,
    record_version INTEGER NOT NULL CHECK (record_version > 0),
    PRIMARY KEY (actor_digest, scope_digest, request_id)
);
CREATE TRIGGER IF NOT EXISTS enterprise_policy_exception_versions_no_update
BEFORE UPDATE ON enterprise_policy_exception_versions
BEGIN
    SELECT RAISE(ABORT, 'enterprise Policy exception versions are immutable');
END;
CREATE TRIGGER IF NOT EXISTS enterprise_policy_exception_versions_no_delete
BEFORE DELETE ON enterprise_policy_exception_versions
BEGIN
    SELECT RAISE(ABORT, 'enterprise Policy exception versions are immutable');
END;
CREATE TRIGGER IF NOT EXISTS enterprise_policy_evaluation_audit_no_update
BEFORE UPDATE ON enterprise_policy_evaluation_audit
BEGIN
    SELECT RAISE(ABORT, 'enterprise Policy evaluation audit is immutable');
END;
CREATE TRIGGER IF NOT EXISTS enterprise_policy_evaluation_audit_no_delete
BEFORE DELETE ON enterprise_policy_evaluation_audit
BEGIN
    SELECT RAISE(ABORT, 'enterprise Policy evaluation audit is immutable');
END;
CREATE TRIGGER IF NOT EXISTS enterprise_policy_evaluation_receipts_no_update
BEFORE UPDATE ON enterprise_policy_evaluation_receipts
BEGIN
    SELECT RAISE(ABORT, 'enterprise Policy evaluation receipts are immutable');
END;
CREATE TRIGGER IF NOT EXISTS enterprise_policy_evaluation_receipts_no_delete
BEFORE DELETE ON enterprise_policy_evaluation_receipts
BEGIN
    SELECT RAISE(ABORT, 'enterprise Policy evaluation receipts are immutable');
END;
";

/// Canonical identifier of one exception workflow.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(transparent)]
pub struct EnterprisePolicyExceptionId(pub String);

/// Immutable input facts presented to the pure evaluator.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EnterprisePolicyEvaluationInput {
    pub scope: EnterprisePolicyScope,
    pub policy_kind: EnterprisePolicyKind,
    pub resource: String,
    pub subject_sha256: Sha256Digest,
    pub matched_condition_sha256: Vec<Sha256Digest>,
    pub evaluated_at: Instant,
}

/// One evaluation, optionally bound to an existing exception.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EnterprisePolicyEvaluationRequest {
    pub input: EnterprisePolicyEvaluationInput,
    pub exception_id: Option<EnterprisePolicyExceptionId>,
}

/// Closed evaluator outcomes. The evaluator itself never performs the action.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EnterprisePolicyEvaluationOutcome {
    Allow,
    Deny,
    RequireApproval,
    Escalate,
}

/// Stable explanation category for one outcome.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EnterprisePolicyEvaluationReason {
    NoActivePolicy,
    ExplicitAllow,
    ExplicitDeny,
    DefaultAllow,
    DefaultDeny,
    ExceptionPending,
    ExceptionApproved,
    ExceptionRejected,
    ExceptionEscalated,
    ExceptionExpired,
}

/// Durable exception state. Expiration is derived from the frozen clock and is
/// intentionally not another mutable state.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EnterprisePolicyExceptionState {
    Pending,
    Approved,
    Rejected,
    Escalated,
}

impl EnterprisePolicyExceptionState {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Approved => "approved",
            Self::Rejected => "rejected",
            Self::Escalated => "escalated",
        }
    }
}

/// Exact exception version used by an evaluation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EnterprisePolicyExceptionReference {
    pub exception_id: EnterprisePolicyExceptionId,
    pub version: u64,
    pub revision: u64,
    pub state: EnterprisePolicyExceptionState,
    pub expires_at: Instant,
    pub record_sha256: Sha256Digest,
}

/// Complete deterministic evaluation result.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EnterprisePolicyEvaluation {
    pub outcome: EnterprisePolicyEvaluationOutcome,
    pub reason: EnterprisePolicyEvaluationReason,
    pub policy_mode: Option<EnterprisePolicyMode>,
    pub policy_version: Option<EnterprisePolicyVersionReference>,
    pub matched_rule: Option<EnterprisePolicyRule>,
    pub hard_invariant: bool,
    pub exception: Option<EnterprisePolicyExceptionReference>,
    pub input_sha256: Sha256Digest,
    pub decision_sha256: Sha256Digest,
    pub evaluated_at: Instant,
}

/// Authenticated command that durably audits one evaluation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EnterprisePolicyEvaluationCommand {
    pub request: EnterprisePolicyEvaluationRequest,
    pub actor: EnterprisePolicyActor,
    pub request_id: RequestId,
}

/// One immutable evaluation audit record.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EnterprisePolicyEvaluationAudit {
    pub sequence: u64,
    pub actor: EnterprisePolicyActor,
    pub request_id: RequestId,
    pub request: EnterprisePolicyEvaluationRequest,
    pub decision: EnterprisePolicyEvaluation,
}

/// Durable evaluation receipt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EnterprisePolicyEvaluationReceipt {
    pub audit: EnterprisePolicyEvaluationAudit,
    pub idempotent_replay: bool,
}

/// Request to open a bounded exception for one exact default-deny decision.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EnterprisePolicyExceptionRequest {
    pub exception_id: EnterprisePolicyExceptionId,
    pub input: EnterprisePolicyEvaluationInput,
    pub justification_sha256: Sha256Digest,
    pub expires_at: Instant,
    pub actor: EnterprisePolicyActor,
    pub request_id: RequestId,
}

/// Human decision applied to a pending or escalated exception.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EnterprisePolicyExceptionDecision {
    Approve,
    Reject,
    Escalate,
}

/// Authenticated exception transition.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EnterprisePolicyExceptionDecisionCommand {
    pub exception_id: EnterprisePolicyExceptionId,
    pub scope: EnterprisePolicyScope,
    pub expected_revision: u64,
    pub decision: EnterprisePolicyExceptionDecision,
    pub actor: EnterprisePolicyActor,
    pub request_id: RequestId,
    pub decided_at: Instant,
}

/// One immutable exception version.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EnterprisePolicyExceptionVersion {
    pub exception_id: EnterprisePolicyExceptionId,
    pub version: u64,
    pub revision: u64,
    pub scope: EnterprisePolicyScope,
    pub policy_kind: EnterprisePolicyKind,
    pub input_sha256: Sha256Digest,
    pub policy_version: EnterprisePolicyVersionReference,
    pub justification_sha256: Sha256Digest,
    pub state: EnterprisePolicyExceptionState,
    pub requested_by: EnterprisePolicyActor,
    pub requested_at: Instant,
    pub expires_at: Instant,
    pub decided_by: Option<EnterprisePolicyActor>,
    pub decided_at: Option<Instant>,
    pub source_request_id: RequestId,
    pub record_sha256: Sha256Digest,
}

impl EnterprisePolicyExceptionVersion {
    #[must_use]
    pub fn reference(&self) -> EnterprisePolicyExceptionReference {
        EnterprisePolicyExceptionReference {
            exception_id: self.exception_id.clone(),
            version: self.version,
            revision: self.revision,
            state: self.state,
            expires_at: self.expires_at.clone(),
            record_sha256: self.record_sha256.clone(),
        }
    }
}

/// Durable exception mutation receipt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EnterprisePolicyExceptionReceipt {
    pub version: EnterprisePolicyExceptionVersion,
    pub previous_revision: u64,
    pub idempotent_replay: bool,
}

/// Stable audit cursor with a fixed upper bound.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EnterprisePolicyEvaluationAuditCursor {
    pub snapshot_sequence: u64,
    pub after_sequence: u64,
}

/// Bounded audit page.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EnterprisePolicyEvaluationAuditPage {
    pub entries: Vec<EnterprisePolicyEvaluationAudit>,
    pub next: Option<EnterprisePolicyEvaluationAuditCursor>,
}

/// Stable error categories.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EnterprisePolicyEvaluationErrorKind {
    InvalidInput,
    RevisionConflict,
    RequestConflict,
    AuthorityMismatch,
    HardInvariant,
    NotFound,
    CorruptState,
    Storage,
}

/// Secret-free evaluation/exception error.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EnterprisePolicyEvaluationError {
    kind: EnterprisePolicyEvaluationErrorKind,
    message: String,
}

impl EnterprisePolicyEvaluationError {
    #[must_use]
    pub const fn kind(&self) -> EnterprisePolicyEvaluationErrorKind {
        self.kind
    }
}

impl fmt::Display for EnterprisePolicyEvaluationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for EnterprisePolicyEvaluationError {}

/// SQLite-backed evaluator, exception ledger, and audit ledger.
pub struct EnterprisePolicyEvaluationLedger<'storage> {
    storage: &'storage mut SqliteStorage,
}

impl SqliteStorage {
    /// Opens the enterprise Policy evaluation and exception ledger.
    ///
    /// # Errors
    ///
    /// Rejects an unavailable connection or non-canonical durable schema.
    pub fn enterprise_policy_evaluation_ledger(
        &mut self,
    ) -> Result<EnterprisePolicyEvaluationLedger<'_>, EnterprisePolicyEvaluationError> {
        EnterprisePolicyEvaluationLedger::new(self)
    }
}

impl<'storage> EnterprisePolicyEvaluationLedger<'storage> {
    fn new(storage: &'storage mut SqliteStorage) -> Result<Self, EnterprisePolicyEvaluationError> {
        let connection = storage
            .connection()
            .map_err(|error| storage_error(&error))?;
        connection
            .execute_batch(EVALUATION_SCHEMA)
            .map_err(|error| sql_error(&error))?;
        validate_schema(connection)?;
        Ok(Self { storage })
    }

    /// Evaluates through the exact production evaluator without persisting a
    /// receipt, audit record, or exception mutation.
    ///
    /// # Errors
    ///
    /// Rejects invalid input, stale/foreign exceptions, corrupt Policy facts,
    /// or storage failures.
    pub fn dry_run(
        &mut self,
        request: &EnterprisePolicyEvaluationRequest,
    ) -> Result<EnterprisePolicyEvaluation, EnterprisePolicyEvaluationError> {
        validate_evaluation_request(request)?;
        let transaction = self
            .storage
            .connection_mut()
            .map_err(|error| storage_error(&error))?
            .transaction_with_behavior(TransactionBehavior::Deferred)
            .map_err(|error| sql_error(&error))?;
        let decision = evaluate_in_connection(&transaction, request)?;
        transaction.rollback().map_err(|error| sql_error(&error))?;
        Ok(decision)
    }

    /// Evaluates and atomically appends one immutable audit receipt.
    ///
    /// # Errors
    ///
    /// Rejects invalid input, changed request reuse, stale/foreign exceptions,
    /// corrupt Policy facts, or storage failures.
    pub fn evaluate(
        &mut self,
        command: &EnterprisePolicyEvaluationCommand,
    ) -> Result<EnterprisePolicyEvaluationReceipt, EnterprisePolicyEvaluationError> {
        validate_evaluation_command(command)?;
        let actor_digest = canonical_digest(&command.actor)?;
        let scope_digest = canonical_digest(&command.request.input.scope)?;
        let command_digest = evaluation_command_digest(command)?;
        let transaction = self.immediate_transaction()?;
        if let Some(receipt) = load_receipt(
            &transaction,
            &actor_digest,
            &scope_digest,
            &command.request_id,
        )? {
            return replay_evaluation(transaction, &receipt, &command_digest);
        }
        let decision = evaluate_in_connection(&transaction, &command.request)?;
        let sequence = next_audit_sequence(&transaction)?;
        let audit = EnterprisePolicyEvaluationAudit {
            sequence,
            actor: command.actor.clone(),
            request_id: command.request_id.clone(),
            request: command.request.clone(),
            decision,
        };
        insert_audit(&transaction, &audit, &scope_digest)?;
        insert_receipt(
            &transaction,
            &ReceiptWrite {
                actor_digest: &actor_digest,
                scope_digest: &scope_digest,
                request_id: &command.request_id,
                command_kind: "evaluate",
                command_digest: &command_digest,
                record_kind: "audit",
                record_id: &format!("audit:{sequence}"),
                record_version: sequence,
            },
        )?;
        transaction.commit().map_err(|error| sql_error(&error))?;
        Ok(EnterprisePolicyEvaluationReceipt {
            audit,
            idempotent_replay: false,
        })
    }

    /// Opens a pending exception bound to one exact default-deny decision.
    /// Explicit deny rules are hard invariants and cannot open an exception.
    ///
    /// # Errors
    ///
    /// Rejects allowing decisions, explicit denies, invalid expiry, changed
    /// request reuse, duplicate exception identity, corruption, or storage failure.
    pub fn request_exception(
        &mut self,
        command: &EnterprisePolicyExceptionRequest,
    ) -> Result<EnterprisePolicyExceptionReceipt, EnterprisePolicyEvaluationError> {
        validate_exception_request(command)?;
        let actor_digest = canonical_digest(&command.actor)?;
        let scope_digest = canonical_digest(&command.input.scope)?;
        let command_digest = exception_request_digest(command)?;
        let transaction = self.immediate_transaction()?;
        if let Some(receipt) = load_receipt(
            &transaction,
            &actor_digest,
            &scope_digest,
            &command.request_id,
        )? {
            return replay_exception(transaction, &receipt, &command_digest, 0);
        }
        if load_exception_head(&transaction, &command.exception_id)?.is_some() {
            return Err(error(
                EnterprisePolicyEvaluationErrorKind::AuthorityMismatch,
                "enterprise Policy exception id already belongs to another request",
            ));
        }
        let base_request = EnterprisePolicyEvaluationRequest {
            input: command.input.clone(),
            exception_id: None,
        };
        let decision = evaluate_in_connection(&transaction, &base_request)?;
        if decision.hard_invariant {
            return Err(error(
                EnterprisePolicyEvaluationErrorKind::HardInvariant,
                "enterprise Policy explicit deny is not exception-eligible",
            ));
        }
        if decision.outcome != EnterprisePolicyEvaluationOutcome::Deny {
            return Err(error(
                EnterprisePolicyEvaluationErrorKind::InvalidInput,
                "enterprise Policy exception requires one exact default-deny decision",
            ));
        }
        let policy_version = decision.policy_version.ok_or_else(|| {
            error(
                EnterprisePolicyEvaluationErrorKind::InvalidInput,
                "enterprise Policy exception requires an active Policy version",
            )
        })?;
        let mut version = EnterprisePolicyExceptionVersion {
            exception_id: command.exception_id.clone(),
            version: 1,
            revision: 1,
            scope: command.input.scope.clone(),
            policy_kind: command.input.policy_kind,
            input_sha256: decision.input_sha256,
            policy_version,
            justification_sha256: command.justification_sha256.clone(),
            state: EnterprisePolicyExceptionState::Pending,
            requested_by: command.actor.clone(),
            requested_at: command.input.evaluated_at.clone(),
            expires_at: command.expires_at.clone(),
            decided_by: None,
            decided_at: None,
            source_request_id: command.request_id.clone(),
            record_sha256: empty_digest(),
        };
        version.record_sha256 = exception_record_digest(&version)?;
        insert_exception_version(&transaction, &version, &scope_digest)?;
        upsert_exception_head(&transaction, &version, false)?;
        insert_receipt(
            &transaction,
            &ReceiptWrite {
                actor_digest: &actor_digest,
                scope_digest: &scope_digest,
                request_id: &command.request_id,
                command_kind: "exception_request",
                command_digest: &command_digest,
                record_kind: "exception",
                record_id: &command.exception_id.0,
                record_version: 1,
            },
        )?;
        transaction.commit().map_err(|error| sql_error(&error))?;
        Ok(EnterprisePolicyExceptionReceipt {
            version,
            previous_revision: 0,
            idempotent_replay: false,
        })
    }

    /// Applies one human approval, rejection, or escalation.
    ///
    /// # Errors
    ///
    /// Rejects non-user actors, stale revisions, expired/terminal exceptions,
    /// changed request reuse, corruption, or storage failure.
    pub fn decide_exception(
        &mut self,
        command: &EnterprisePolicyExceptionDecisionCommand,
    ) -> Result<EnterprisePolicyExceptionReceipt, EnterprisePolicyEvaluationError> {
        validate_exception_decision(command)?;
        let actor_digest = canonical_digest(&command.actor)?;
        let scope_digest = canonical_digest(&command.scope)?;
        let command_digest = exception_decision_digest(command)?;
        let transaction = self.immediate_transaction()?;
        if let Some(receipt) = load_receipt(
            &transaction,
            &actor_digest,
            &scope_digest,
            &command.request_id,
        )? {
            return replay_exception(
                transaction,
                &receipt,
                &command_digest,
                command.expected_revision,
            );
        }
        let current =
            load_exception_head(&transaction, &command.exception_id)?.ok_or_else(|| {
                error(
                    EnterprisePolicyEvaluationErrorKind::NotFound,
                    "enterprise Policy exception does not exist",
                )
            })?;
        if current.scope != command.scope {
            return Err(error(
                EnterprisePolicyEvaluationErrorKind::AuthorityMismatch,
                "enterprise Policy exception belongs to another scope",
            ));
        }
        if current.revision != command.expected_revision {
            return Err(error(
                EnterprisePolicyEvaluationErrorKind::RevisionConflict,
                "enterprise Policy exception expected revision is stale",
            ));
        }
        if command.decided_at.0 >= current.expires_at.0 {
            return Err(error(
                EnterprisePolicyEvaluationErrorKind::AuthorityMismatch,
                "enterprise Policy exception has expired",
            ));
        }
        validate_exception_transition(current.state, command.decision)?;
        let state = match command.decision {
            EnterprisePolicyExceptionDecision::Approve => EnterprisePolicyExceptionState::Approved,
            EnterprisePolicyExceptionDecision::Reject => EnterprisePolicyExceptionState::Rejected,
            EnterprisePolicyExceptionDecision::Escalate => {
                EnterprisePolicyExceptionState::Escalated
            }
        };
        let (next_version, next_revision) = next_exception_coordinates(&current)?;
        let mut version = EnterprisePolicyExceptionVersion {
            exception_id: current.exception_id.clone(),
            version: next_version,
            revision: next_revision,
            scope: current.scope.clone(),
            policy_kind: current.policy_kind,
            input_sha256: current.input_sha256.clone(),
            policy_version: current.policy_version.clone(),
            justification_sha256: current.justification_sha256.clone(),
            state,
            requested_by: current.requested_by.clone(),
            requested_at: current.requested_at.clone(),
            expires_at: current.expires_at.clone(),
            decided_by: Some(command.actor.clone()),
            decided_at: Some(command.decided_at.clone()),
            source_request_id: command.request_id.clone(),
            record_sha256: empty_digest(),
        };
        version.record_sha256 = exception_record_digest(&version)?;
        insert_exception_version(&transaction, &version, &scope_digest)?;
        upsert_exception_head(&transaction, &version, true)?;
        insert_receipt(
            &transaction,
            &ReceiptWrite {
                actor_digest: &actor_digest,
                scope_digest: &scope_digest,
                request_id: &command.request_id,
                command_kind: "exception_decide",
                command_digest: &command_digest,
                record_kind: "exception",
                record_id: &command.exception_id.0,
                record_version: version.version,
            },
        )?;
        transaction.commit().map_err(|error| sql_error(&error))?;
        Ok(EnterprisePolicyExceptionReceipt {
            version,
            previous_revision: current.revision,
            idempotent_replay: false,
        })
    }

    /// Loads the current immutable exception head.
    ///
    /// # Errors
    ///
    /// Rejects invalid identity, canonical byte drift, or storage failure.
    pub fn load_exception(
        &self,
        exception_id: &EnterprisePolicyExceptionId,
    ) -> Result<Option<EnterprisePolicyExceptionVersion>, EnterprisePolicyEvaluationError> {
        validate_exception_id(exception_id)?;
        load_exception_head(
            self.storage
                .connection()
                .map_err(|error| storage_error(&error))?,
            exception_id,
        )
    }

    /// Enumerates one bounded immutable exception history.
    ///
    /// # Errors
    ///
    /// Rejects invalid coordinates, canonical byte drift, or storage failure.
    pub fn scan_exception_versions(
        &self,
        exception_id: &EnterprisePolicyExceptionId,
        after_version: u64,
        limit: u64,
    ) -> Result<Vec<EnterprisePolicyExceptionVersion>, EnterprisePolicyEvaluationError> {
        validate_exception_id(exception_id)?;
        validate_page(after_version, limit)?;
        let connection = self
            .storage
            .connection()
            .map_err(|error| storage_error(&error))?;
        let mut statement = connection
            .prepare(
                "SELECT version FROM enterprise_policy_exception_versions
                 WHERE exception_id = ?1 AND version > ?2
                 ORDER BY version ASC LIMIT ?3",
            )
            .map_err(|error| sql_error(&error))?;
        let coordinates = statement
            .query_map(
                params![
                    exception_id.0,
                    sql_integer(after_version)?,
                    sql_integer(limit)?
                ],
                |row| row.get::<_, i64>(0),
            )
            .map_err(|error| sql_error(&error))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| sql_error(&error))?;
        coordinates
            .into_iter()
            .map(|version| {
                load_exception_version(
                    connection,
                    exception_id,
                    from_sql_positive(version, "enterprise Policy exception version")?,
                )
            })
            .collect()
    }

    /// Scans one stable, bounded evaluation audit snapshot.
    ///
    /// # Errors
    ///
    /// Rejects invalid cursors, canonical byte drift, or storage failure.
    pub fn scan_audit(
        &self,
        cursor: Option<&EnterprisePolicyEvaluationAuditCursor>,
        limit: u64,
    ) -> Result<EnterprisePolicyEvaluationAuditPage, EnterprisePolicyEvaluationError> {
        if limit == 0 || limit > MAX_PAGE_SIZE {
            return Err(invalid("enterprise Policy audit limit is outside 1..=200"));
        }
        let connection = self
            .storage
            .connection()
            .map_err(|error| storage_error(&error))?;
        let snapshot_sequence = match cursor {
            Some(cursor) => {
                if cursor.after_sequence > cursor.snapshot_sequence
                    || cursor.snapshot_sequence > MAX_SAFE_INTEGER
                {
                    return Err(invalid("enterprise Policy audit cursor is invalid"));
                }
                cursor.snapshot_sequence
            }
            None => last_audit_sequence(connection)?,
        };
        let after_sequence = cursor.map_or(0, |cursor| cursor.after_sequence);
        let mut statement = connection
            .prepare(
                "SELECT sequence FROM enterprise_policy_evaluation_audit
                 WHERE sequence > ?1 AND sequence <= ?2
                 ORDER BY sequence ASC LIMIT ?3",
            )
            .map_err(|error| sql_error(&error))?;
        let coordinates = statement
            .query_map(
                params![
                    sql_integer(after_sequence)?,
                    sql_integer(snapshot_sequence)?,
                    sql_integer(limit + 1)?,
                ],
                |row| row.get::<_, i64>(0),
            )
            .map_err(|error| sql_error(&error))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| sql_error(&error))?;
        let page_size = usize::try_from(limit).map_err(|_| invalid("invalid audit page size"))?;
        let has_more = coordinates.len() > page_size;
        let entries = coordinates
            .into_iter()
            .take(page_size)
            .map(|sequence| {
                load_audit(
                    connection,
                    from_sql_positive(sequence, "enterprise Policy audit sequence")?,
                )
            })
            .collect::<Result<Vec<_>, _>>()?;
        let next = has_more.then(|| EnterprisePolicyEvaluationAuditCursor {
            snapshot_sequence,
            after_sequence: entries
                .last()
                .map_or(after_sequence, |entry| entry.sequence),
        });
        Ok(EnterprisePolicyEvaluationAuditPage { entries, next })
    }

    fn immediate_transaction(
        &mut self,
    ) -> Result<Transaction<'_>, EnterprisePolicyEvaluationError> {
        self.storage
            .connection_mut()
            .map_err(|error| storage_error(&error))?
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| sql_error(&error))
    }
}

fn next_exception_coordinates(
    current: &EnterprisePolicyExceptionVersion,
) -> Result<(u64, u64), EnterprisePolicyEvaluationError> {
    let version = current
        .version
        .checked_add(1)
        .filter(|version| *version <= MAX_SAFE_INTEGER)
        .ok_or_else(|| corrupt("enterprise Policy exception version overflows"))?;
    let revision = current
        .revision
        .checked_add(1)
        .filter(|revision| *revision <= MAX_SAFE_INTEGER)
        .ok_or_else(|| corrupt("enterprise Policy exception revision overflows"))?;
    Ok((version, revision))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct EvaluationTargetFacts<'a> {
    scope: &'a EnterprisePolicyScope,
    policy_kind: EnterprisePolicyKind,
    resource: &'a str,
    subject_sha256: &'a Sha256Digest,
    matched_condition_sha256: &'a [Sha256Digest],
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct EvaluationDecisionFacts<'a> {
    outcome: EnterprisePolicyEvaluationOutcome,
    reason: EnterprisePolicyEvaluationReason,
    policy_mode: Option<EnterprisePolicyMode>,
    policy_version: &'a Option<EnterprisePolicyVersionReference>,
    matched_rule: &'a Option<EnterprisePolicyRule>,
    hard_invariant: bool,
    exception: &'a Option<EnterprisePolicyExceptionReference>,
    input_sha256: &'a Sha256Digest,
    evaluated_at: &'a Instant,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct EvaluationCommandFacts<'a> {
    input: EvaluationTargetFacts<'a>,
    exception_id: &'a Option<EnterprisePolicyExceptionId>,
    actor: &'a EnterprisePolicyActor,
    request_id: &'a RequestId,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ExceptionRequestFacts<'a> {
    exception_id: &'a EnterprisePolicyExceptionId,
    input: EvaluationTargetFacts<'a>,
    justification_sha256: &'a Sha256Digest,
    expires_at: &'a Instant,
    actor: &'a EnterprisePolicyActor,
    request_id: &'a RequestId,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ExceptionDecisionFacts<'a> {
    exception_id: &'a EnterprisePolicyExceptionId,
    scope: &'a EnterprisePolicyScope,
    expected_revision: u64,
    decision: EnterprisePolicyExceptionDecision,
    actor: &'a EnterprisePolicyActor,
    request_id: &'a RequestId,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ExceptionRecordFacts<'a> {
    exception_id: &'a EnterprisePolicyExceptionId,
    version: u64,
    revision: u64,
    scope: &'a EnterprisePolicyScope,
    policy_kind: EnterprisePolicyKind,
    input_sha256: &'a Sha256Digest,
    policy_version: &'a EnterprisePolicyVersionReference,
    justification_sha256: &'a Sha256Digest,
    state: EnterprisePolicyExceptionState,
    requested_by: &'a EnterprisePolicyActor,
    requested_at: &'a Instant,
    expires_at: &'a Instant,
    decided_by: &'a Option<EnterprisePolicyActor>,
    decided_at: &'a Option<Instant>,
    source_request_id: &'a RequestId,
}

struct StoredReceipt {
    command_kind: String,
    command_digest: String,
    record_kind: String,
    record_id: String,
    record_version: u64,
}

struct ReceiptWrite<'a> {
    actor_digest: &'a str,
    scope_digest: &'a str,
    request_id: &'a RequestId,
    command_kind: &'a str,
    command_digest: &'a str,
    record_kind: &'a str,
    record_id: &'a str,
    record_version: u64,
}

struct BasePolicyEvaluation {
    effect: EnterprisePolicyEffect,
    matched_rule: Option<EnterprisePolicyRule>,
    hard_invariant: bool,
    policy_mode: Option<EnterprisePolicyMode>,
    policy_version: Option<EnterprisePolicyVersionReference>,
    reason: EnterprisePolicyEvaluationReason,
}

fn evaluate_in_connection(
    connection: &Connection,
    request: &EnterprisePolicyEvaluationRequest,
) -> Result<EnterprisePolicyEvaluation, EnterprisePolicyEvaluationError> {
    let input_sha256 = evaluation_input_digest(&request.input)?;
    let policy = resolve_effective_policy(
        connection,
        &request.input.scope,
        request.input.policy_kind,
        &request.input.evaluated_at,
    )
    .map_err(|error| policy_error(&error))?;
    let base = base_policy_evaluation(policy.as_ref(), &request.input);
    let mut exception_reference = None;
    let (outcome, reason) = if base.effect == EnterprisePolicyEffect::Allow {
        if request.exception_id.is_some() {
            return Err(error(
                EnterprisePolicyEvaluationErrorKind::AuthorityMismatch,
                "enterprise Policy exception cannot relabel an allowing decision",
            ));
        }
        (EnterprisePolicyEvaluationOutcome::Allow, base.reason)
    } else if base.hard_invariant {
        if request.exception_id.is_some() {
            return Err(error(
                EnterprisePolicyEvaluationErrorKind::HardInvariant,
                "enterprise Policy explicit deny cannot be bypassed by an exception",
            ));
        }
        (EnterprisePolicyEvaluationOutcome::Deny, base.reason)
    } else if let Some(exception_id) = &request.exception_id {
        let exception = load_exception_head(connection, exception_id)?.ok_or_else(|| {
            error(
                EnterprisePolicyEvaluationErrorKind::NotFound,
                "enterprise Policy exception does not exist",
            )
        })?;
        validate_exception_binding(
            &exception,
            &request.input,
            &input_sha256,
            base.policy_version.as_ref(),
        )?;
        exception_reference = Some(exception.reference());
        exception_outcome(&exception, &request.input.evaluated_at)
    } else {
        (EnterprisePolicyEvaluationOutcome::Deny, base.reason)
    };
    let mut decision = EnterprisePolicyEvaluation {
        outcome,
        reason,
        policy_mode: base.policy_mode,
        policy_version: base.policy_version,
        matched_rule: base.matched_rule,
        hard_invariant: base.hard_invariant,
        exception: exception_reference,
        input_sha256,
        decision_sha256: empty_digest(),
        evaluated_at: request.input.evaluated_at.clone(),
    };
    decision.decision_sha256 = evaluation_decision_digest(&decision)?;
    Ok(decision)
}

fn base_policy_evaluation(
    policy: Option<&EnterprisePolicyVersion>,
    input: &EnterprisePolicyEvaluationInput,
) -> BasePolicyEvaluation {
    let Some(policy) = policy else {
        return BasePolicyEvaluation {
            effect: EnterprisePolicyEffect::Allow,
            matched_rule: None,
            hard_invariant: false,
            policy_mode: None,
            policy_version: None,
            reason: EnterprisePolicyEvaluationReason::NoActivePolicy,
        };
    };
    let matched = matched_rule(policy, input);
    let (effect, hard_invariant, reason) = match matched {
        Some(rule) if rule.effect == EnterprisePolicyEffect::Deny => (
            EnterprisePolicyEffect::Deny,
            true,
            EnterprisePolicyEvaluationReason::ExplicitDeny,
        ),
        Some(_) => (
            EnterprisePolicyEffect::Allow,
            false,
            EnterprisePolicyEvaluationReason::ExplicitAllow,
        ),
        None if policy.definition.default_effect == EnterprisePolicyEffect::Deny => (
            EnterprisePolicyEffect::Deny,
            false,
            EnterprisePolicyEvaluationReason::DefaultDeny,
        ),
        None => (
            EnterprisePolicyEffect::Allow,
            false,
            EnterprisePolicyEvaluationReason::DefaultAllow,
        ),
    };
    BasePolicyEvaluation {
        effect,
        matched_rule: matched.cloned(),
        hard_invariant,
        policy_mode: Some(policy.mode),
        policy_version: Some(policy.reference()),
        reason,
    }
}

fn matched_rule<'policy>(
    policy: &'policy EnterprisePolicyVersion,
    input: &EnterprisePolicyEvaluationInput,
) -> Option<&'policy EnterprisePolicyRule> {
    let matches = |rule: &&EnterprisePolicyRule| {
        input
            .matched_condition_sha256
            .contains(&rule.condition_sha256)
            && resource_matches(&rule.resource_pattern, &input.resource)
    };
    policy
        .definition
        .rules
        .iter()
        .filter(matches)
        .find(|rule| rule.effect == EnterprisePolicyEffect::Deny)
        .or_else(|| policy.definition.rules.iter().find(matches))
}

fn resource_matches(pattern: &str, resource: &str) -> bool {
    let pattern = pattern.chars().collect::<Vec<_>>();
    let resource = resource.chars().collect::<Vec<_>>();
    let (mut pattern_index, mut resource_index) = (0, 0);
    let (mut star_index, mut star_resource_index) = (None, 0);
    while resource_index < resource.len() {
        if pattern
            .get(pattern_index)
            .is_some_and(|value| *value == resource[resource_index])
        {
            pattern_index += 1;
            resource_index += 1;
        } else if pattern.get(pattern_index) == Some(&'*') {
            star_index = Some(pattern_index);
            pattern_index += 1;
            star_resource_index = resource_index;
        } else if let Some(star) = star_index {
            star_resource_index += 1;
            resource_index = star_resource_index;
            pattern_index = star + 1;
        } else {
            return false;
        }
    }
    while pattern.get(pattern_index) == Some(&'*') {
        pattern_index += 1;
    }
    pattern_index == pattern.len()
}

fn exception_outcome(
    exception: &EnterprisePolicyExceptionVersion,
    evaluated_at: &Instant,
) -> (
    EnterprisePolicyEvaluationOutcome,
    EnterprisePolicyEvaluationReason,
) {
    if evaluated_at.0 >= exception.expires_at.0 {
        return (
            EnterprisePolicyEvaluationOutcome::Deny,
            EnterprisePolicyEvaluationReason::ExceptionExpired,
        );
    }
    match exception.state {
        EnterprisePolicyExceptionState::Pending => (
            EnterprisePolicyEvaluationOutcome::RequireApproval,
            EnterprisePolicyEvaluationReason::ExceptionPending,
        ),
        EnterprisePolicyExceptionState::Approved => (
            EnterprisePolicyEvaluationOutcome::Allow,
            EnterprisePolicyEvaluationReason::ExceptionApproved,
        ),
        EnterprisePolicyExceptionState::Rejected => (
            EnterprisePolicyEvaluationOutcome::Deny,
            EnterprisePolicyEvaluationReason::ExceptionRejected,
        ),
        EnterprisePolicyExceptionState::Escalated => (
            EnterprisePolicyEvaluationOutcome::Escalate,
            EnterprisePolicyEvaluationReason::ExceptionEscalated,
        ),
    }
}

fn validate_exception_binding(
    exception: &EnterprisePolicyExceptionVersion,
    input: &EnterprisePolicyEvaluationInput,
    input_sha256: &Sha256Digest,
    policy_version: Option<&EnterprisePolicyVersionReference>,
) -> Result<(), EnterprisePolicyEvaluationError> {
    if exception.scope != input.scope
        || exception.policy_kind != input.policy_kind
        || exception.input_sha256 != *input_sha256
        || Some(&exception.policy_version) != policy_version
    {
        return Err(error(
            EnterprisePolicyEvaluationErrorKind::AuthorityMismatch,
            "enterprise Policy exception belongs to another input or Policy version",
        ));
    }
    Ok(())
}

fn validate_evaluation_request(
    request: &EnterprisePolicyEvaluationRequest,
) -> Result<(), EnterprisePolicyEvaluationError> {
    validate_input(&request.input)?;
    if let Some(exception_id) = &request.exception_id {
        validate_exception_id(exception_id)?;
    }
    Ok(())
}

fn validate_evaluation_command(
    command: &EnterprisePolicyEvaluationCommand,
) -> Result<(), EnterprisePolicyEvaluationError> {
    validate_evaluation_request(&command.request)?;
    validate_actor(&command.actor)?;
    validate_id(
        &command.request_id.0,
        "req_",
        "enterprise Policy evaluation request",
    )
}

fn validate_exception_request(
    command: &EnterprisePolicyExceptionRequest,
) -> Result<(), EnterprisePolicyEvaluationError> {
    validate_exception_id(&command.exception_id)?;
    validate_input(&command.input)?;
    validate_digest(
        &command.justification_sha256,
        "enterprise Policy exception justification",
    )?;
    validate_actor(&command.actor)?;
    validate_id(
        &command.request_id.0,
        "req_",
        "enterprise Policy exception request",
    )?;
    validate_instant(&command.expires_at, "enterprise Policy exception expiresAt")?;
    if command.expires_at.0 <= command.input.evaluated_at.0 {
        return Err(invalid(
            "enterprise Policy exception expiry must follow its trusted request time",
        ));
    }
    Ok(())
}

fn validate_exception_decision(
    command: &EnterprisePolicyExceptionDecisionCommand,
) -> Result<(), EnterprisePolicyEvaluationError> {
    validate_exception_id(&command.exception_id)?;
    validate_scope(&command.scope)?;
    validate_user_actor(&command.actor)?;
    validate_id(
        &command.request_id.0,
        "req_",
        "enterprise Policy exception decision request",
    )?;
    validate_instant(&command.decided_at, "enterprise Policy exception decidedAt")?;
    if command.expected_revision == 0 || command.expected_revision > MAX_SAFE_INTEGER {
        return Err(invalid(
            "enterprise Policy exception expected revision is invalid",
        ));
    }
    Ok(())
}

fn validate_exception_transition(
    current: EnterprisePolicyExceptionState,
    decision: EnterprisePolicyExceptionDecision,
) -> Result<(), EnterprisePolicyEvaluationError> {
    let valid = match current {
        EnterprisePolicyExceptionState::Pending => true,
        EnterprisePolicyExceptionState::Escalated => {
            decision != EnterprisePolicyExceptionDecision::Escalate
        }
        EnterprisePolicyExceptionState::Approved | EnterprisePolicyExceptionState::Rejected => {
            false
        }
    };
    if valid {
        Ok(())
    } else {
        Err(error(
            EnterprisePolicyEvaluationErrorKind::AuthorityMismatch,
            "enterprise Policy exception transition is not allowed",
        ))
    }
}

fn validate_input(
    input: &EnterprisePolicyEvaluationInput,
) -> Result<(), EnterprisePolicyEvaluationError> {
    validate_scope(&input.scope)?;
    if input.resource.is_empty()
        || input.resource.len() > MAX_RESOURCE_BYTES
        || input.resource.chars().any(char::is_control)
    {
        return Err(invalid("enterprise Policy evaluation resource is invalid"));
    }
    validate_digest(
        &input.subject_sha256,
        "enterprise Policy evaluation subject",
    )?;
    validate_instant(&input.evaluated_at, "enterprise Policy evaluatedAt")?;
    if input.matched_condition_sha256.len() > MAX_CONDITIONS
        || input
            .matched_condition_sha256
            .iter()
            .collect::<HashSet<_>>()
            .len()
            != input.matched_condition_sha256.len()
        || !input
            .matched_condition_sha256
            .windows(2)
            .all(|window| window[0].0 < window[1].0)
    {
        return Err(invalid(
            "enterprise Policy matched condition set is invalid or duplicated",
        ));
    }
    for condition in &input.matched_condition_sha256 {
        validate_digest(condition, "enterprise Policy matched condition")?;
    }
    Ok(())
}

fn validate_scope(scope: &EnterprisePolicyScope) -> Result<(), EnterprisePolicyEvaluationError> {
    use EnterprisePolicyScope::{Organization, Project, Repository, Workspace};
    match scope {
        Organization { organization_id } => {
            validate_id(&organization_id.0, "org_", "enterprise Policy organization")
        }
        Workspace {
            organization_id,
            workspace_id,
        } => {
            validate_id(&organization_id.0, "org_", "enterprise Policy organization")?;
            validate_id(&workspace_id.0, "wsp_", "enterprise Policy workspace")
        }
        Project {
            organization_id,
            workspace_id,
            project_id,
        } => {
            validate_id(&organization_id.0, "org_", "enterprise Policy organization")?;
            validate_id(&workspace_id.0, "wsp_", "enterprise Policy workspace")?;
            validate_id(&project_id.0, "prj_", "enterprise Policy project")
        }
        Repository {
            organization_id,
            workspace_id,
            project_id,
            repository_id,
        } => {
            validate_id(&organization_id.0, "org_", "enterprise Policy organization")?;
            validate_id(&workspace_id.0, "wsp_", "enterprise Policy workspace")?;
            validate_id(&project_id.0, "prj_", "enterprise Policy project")?;
            validate_id(&repository_id.0, "rep_", "enterprise Policy repository")
        }
    }
}

fn validate_actor(actor: &EnterprisePolicyActor) -> Result<(), EnterprisePolicyEvaluationError> {
    match actor {
        EnterprisePolicyActor::User { id } => validate_id(&id.0, "usr_", "enterprise Policy user"),
        EnterprisePolicyActor::ServiceAccount { id } => {
            validate_id(&id.0, "svc_", "enterprise Policy service account")
        }
        EnterprisePolicyActor::System { id } => {
            validate_id(&id.0, "sys_", "enterprise Policy system actor")
        }
    }
}

fn validate_user_actor(
    actor: &EnterprisePolicyActor,
) -> Result<(), EnterprisePolicyEvaluationError> {
    if matches!(actor, EnterprisePolicyActor::User { .. }) {
        validate_actor(actor)
    } else {
        Err(error(
            EnterprisePolicyEvaluationErrorKind::AuthorityMismatch,
            "enterprise Policy exception decision requires an authenticated user",
        ))
    }
}

fn validate_exception_id(
    id: &EnterprisePolicyExceptionId,
) -> Result<(), EnterprisePolicyEvaluationError> {
    validate_id(&id.0, "pex_", "enterprise Policy exception id")
}

fn validate_id(
    value: &str,
    prefix: &str,
    field: &str,
) -> Result<(), EnterprisePolicyEvaluationError> {
    let valid = value.strip_prefix(prefix).is_some_and(|suffix| {
        suffix.len() == 26
            && suffix.bytes().all(|byte| {
                byte.is_ascii_digit()
                    || matches!(byte, b'A'..=b'H' | b'J'..=b'K' | b'M'..=b'N' | b'P'..=b'T' | b'V'..=b'Z')
            })
    });
    if valid {
        Ok(())
    } else {
        Err(invalid(format!("{field} is not canonical")))
    }
}

fn validate_digest(
    value: &Sha256Digest,
    field: &str,
) -> Result<(), EnterprisePolicyEvaluationError> {
    let valid = value.0.strip_prefix("sha256:").is_some_and(|suffix| {
        suffix.len() == 64
            && suffix
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    });
    if valid {
        Ok(())
    } else {
        Err(invalid(format!("{field} digest is not canonical")))
    }
}

fn validate_instant(value: &Instant, field: &str) -> Result<(), EnterprisePolicyEvaluationError> {
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
        Err(invalid(format!("{field} is not canonical")))
    }
}

fn validate_page(after: u64, limit: u64) -> Result<(), EnterprisePolicyEvaluationError> {
    if after > MAX_SAFE_INTEGER || limit == 0 || limit > MAX_PAGE_SIZE {
        Err(invalid("enterprise Policy history page is invalid"))
    } else {
        Ok(())
    }
}

fn validate_schema(connection: &Connection) -> Result<(), EnterprisePolicyEvaluationError> {
    for (table, expected) in [
        (
            "enterprise_policy_exception_versions",
            &[
                "sequence",
                "exception_id",
                "version",
                "revision",
                "scope_digest",
                "policy_kind",
                "state",
                "expires_at",
                "record_digest",
                "record_json",
            ][..],
        ),
        (
            "enterprise_policy_exception_heads",
            &["exception_id", "version", "revision", "record_digest"][..],
        ),
        (
            "enterprise_policy_evaluation_audit",
            &[
                "sequence",
                "scope_digest",
                "policy_kind",
                "evaluated_at",
                "input_digest",
                "decision_digest",
                "record_json",
            ][..],
        ),
        (
            "enterprise_policy_evaluation_receipts",
            &[
                "actor_digest",
                "scope_digest",
                "request_id",
                "command_kind",
                "command_digest",
                "record_kind",
                "record_id",
                "record_version",
            ][..],
        ),
    ] {
        let mut statement = connection
            .prepare(&format!("PRAGMA table_info({table})"))
            .map_err(|error| sql_error(&error))?;
        let columns = statement
            .query_map([], |row| row.get::<_, String>(1))
            .map_err(|error| sql_error(&error))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| sql_error(&error))?;
        if columns != expected {
            return Err(corrupt(
                "enterprise Policy evaluation schema is not canonical",
            ));
        }
    }
    require_unique_indexes(
        connection,
        "enterprise_policy_exception_versions",
        &[
            &["exception_id", "version"],
            &["exception_id", "revision"],
            &["record_digest"],
        ],
    )?;
    require_unique_indexes(
        connection,
        "enterprise_policy_exception_heads",
        &[&["exception_id"], &["record_digest"]],
    )?;
    require_unique_indexes(
        connection,
        "enterprise_policy_evaluation_audit",
        &[&["decision_digest"]],
    )?;
    require_unique_indexes(
        connection,
        "enterprise_policy_evaluation_receipts",
        &[&["actor_digest", "scope_digest", "request_id"]],
    )?;
    Ok(())
}

fn require_unique_indexes(
    connection: &Connection,
    table: &str,
    required: &[&[&str]],
) -> Result<(), EnterprisePolicyEvaluationError> {
    let mut statement = connection
        .prepare(&format!("PRAGMA index_list({table})"))
        .map_err(|error| sql_error(&error))?;
    let indexes = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(1)?, row.get::<_, i64>(2)?))
        })
        .map_err(|error| sql_error(&error))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| sql_error(&error))?;
    let mut unique_columns = Vec::new();
    for (name, unique) in indexes {
        if unique == 0 {
            continue;
        }
        let mut index_statement = connection
            .prepare(&format!("PRAGMA index_info({name})"))
            .map_err(|error| sql_error(&error))?;
        unique_columns.push(
            index_statement
                .query_map([], |row| row.get::<_, String>(2))
                .map_err(|error| sql_error(&error))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| sql_error(&error))?,
        );
    }
    if required.iter().all(|expected| {
        unique_columns.iter().any(|actual| {
            actual
                .iter()
                .map(String::as_str)
                .eq(expected.iter().copied())
        })
    }) {
        Ok(())
    } else {
        Err(corrupt(
            "enterprise Policy evaluation uniqueness constraints are not canonical",
        ))
    }
}

fn insert_exception_version(
    transaction: &Transaction<'_>,
    version: &EnterprisePolicyExceptionVersion,
    scope_digest: &str,
) -> Result<(), EnterprisePolicyEvaluationError> {
    let record_json = canonical_json(version)?;
    transaction
        .execute(
            "INSERT INTO enterprise_policy_exception_versions (
                exception_id, version, revision, scope_digest, policy_kind, state,
                expires_at, record_digest, record_json
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                version.exception_id.0,
                sql_integer(version.version)?,
                sql_integer(version.revision)?,
                scope_digest,
                policy_kind_string(version.policy_kind),
                version.state.as_str(),
                version.expires_at.0,
                version.record_sha256.0,
                record_json,
            ],
        )
        .map_err(|error| sql_error(&error))?;
    Ok(())
}

fn upsert_exception_head(
    transaction: &Transaction<'_>,
    version: &EnterprisePolicyExceptionVersion,
    exists: bool,
) -> Result<(), EnterprisePolicyEvaluationError> {
    let changed = if exists {
        transaction
            .execute(
                "UPDATE enterprise_policy_exception_heads
             SET version = ?2, revision = ?3, record_digest = ?4
             WHERE exception_id = ?1 AND revision = ?5",
                params![
                    version.exception_id.0,
                    sql_integer(version.version)?,
                    sql_integer(version.revision)?,
                    version.record_sha256.0,
                    sql_integer(version.revision - 1)?,
                ],
            )
            .map_err(|error| sql_error(&error))?
    } else {
        transaction
            .execute(
                "INSERT INTO enterprise_policy_exception_heads (
                exception_id, version, revision, record_digest
             ) VALUES (?1, ?2, ?3, ?4)",
                params![
                    version.exception_id.0,
                    sql_integer(version.version)?,
                    sql_integer(version.revision)?,
                    version.record_sha256.0,
                ],
            )
            .map_err(|error| sql_error(&error))?
    };
    if changed == 1 {
        Ok(())
    } else {
        Err(error(
            EnterprisePolicyEvaluationErrorKind::RevisionConflict,
            "enterprise Policy exception head changed concurrently",
        ))
    }
}

fn load_exception_head(
    connection: &Connection,
    exception_id: &EnterprisePolicyExceptionId,
) -> Result<Option<EnterprisePolicyExceptionVersion>, EnterprisePolicyEvaluationError> {
    validate_exception_id(exception_id)?;
    let version = connection
        .query_row(
            "SELECT version FROM enterprise_policy_exception_heads WHERE exception_id = ?1",
            [&exception_id.0],
            |row| row.get::<_, i64>(0),
        )
        .optional()
        .map_err(|error| sql_error(&error))?;
    version
        .map(|version| {
            load_exception_version(
                connection,
                exception_id,
                from_sql_positive(version, "enterprise Policy exception head")?,
            )
        })
        .transpose()
}

fn load_exception_version(
    connection: &Connection,
    exception_id: &EnterprisePolicyExceptionId,
    version: u64,
) -> Result<EnterprisePolicyExceptionVersion, EnterprisePolicyEvaluationError> {
    let row = connection
        .query_row(
            "SELECT revision, scope_digest, policy_kind, state, expires_at,
                    record_digest, record_json
             FROM enterprise_policy_exception_versions
             WHERE exception_id = ?1 AND version = ?2",
            params![exception_id.0, sql_integer(version)?],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                ))
            },
        )
        .optional()
        .map_err(|error| sql_error(&error))?
        .ok_or_else(|| {
            error(
                EnterprisePolicyEvaluationErrorKind::NotFound,
                "enterprise Policy exception version does not exist",
            )
        })?;
    let record: EnterprisePolicyExceptionVersion = serde_json::from_str(&row.6)
        .map_err(|_| corrupt("enterprise Policy exception bytes are invalid"))?;
    if canonical_json(&record)? != row.6
        || record.exception_id != *exception_id
        || record.version != version
        || record.revision != from_sql_positive(row.0, "enterprise Policy exception revision")?
        || canonical_digest(&record.scope)? != row.1
        || policy_kind_string(record.policy_kind) != row.2
        || record.state.as_str() != row.3
        || record.expires_at.0 != row.4
        || record.record_sha256.0 != row.5
        || exception_record_digest(&record)? != record.record_sha256
    {
        return Err(corrupt(
            "enterprise Policy exception columns or digest differ from canonical bytes",
        ));
    }
    validate_exception_record(&record)?;
    Ok(record)
}

fn validate_exception_record(
    record: &EnterprisePolicyExceptionVersion,
) -> Result<(), EnterprisePolicyEvaluationError> {
    validate_exception_id(&record.exception_id).map_err(|error| as_corrupt(&error))?;
    validate_scope(&record.scope).map_err(|error| as_corrupt(&error))?;
    validate_actor(&record.requested_by).map_err(|error| as_corrupt(&error))?;
    validate_id(
        &record.source_request_id.0,
        "req_",
        "enterprise Policy exception source request",
    )
    .map_err(|error| as_corrupt(&error))?;
    validate_digest(&record.input_sha256, "enterprise Policy exception input")
        .map_err(|error| as_corrupt(&error))?;
    validate_digest(
        &record.justification_sha256,
        "enterprise Policy exception justification",
    )
    .map_err(|error| as_corrupt(&error))?;
    validate_digest(&record.record_sha256, "enterprise Policy exception record")
        .map_err(|error| as_corrupt(&error))?;
    validate_instant(
        &record.requested_at,
        "enterprise Policy exception requestedAt",
    )
    .map_err(|error| as_corrupt(&error))?;
    validate_instant(&record.expires_at, "enterprise Policy exception expiresAt")
        .map_err(|error| as_corrupt(&error))?;
    if record.expires_at.0 <= record.requested_at.0 || record.version != record.revision {
        return Err(corrupt(
            "enterprise Policy exception version invariants are invalid",
        ));
    }
    if record.state == EnterprisePolicyExceptionState::Pending {
        if record.version != 1 || record.decided_by.is_some() || record.decided_at.is_some() {
            return Err(corrupt(
                "pending enterprise Policy exception has terminal facts",
            ));
        }
    } else {
        let actor = record
            .decided_by
            .as_ref()
            .ok_or_else(|| corrupt("terminal enterprise Policy exception has no actor"))?;
        validate_user_actor(actor).map_err(|error| as_corrupt(&error))?;
        let decided_at = record
            .decided_at
            .as_ref()
            .ok_or_else(|| corrupt("terminal enterprise Policy exception has no time"))?;
        validate_instant(decided_at, "enterprise Policy exception decidedAt")
            .map_err(|error| as_corrupt(&error))?;
        if decided_at.0 < record.requested_at.0 || decided_at.0 >= record.expires_at.0 {
            return Err(corrupt(
                "enterprise Policy exception decision time is outside its authority",
            ));
        }
    }
    Ok(())
}

fn insert_audit(
    transaction: &Transaction<'_>,
    audit: &EnterprisePolicyEvaluationAudit,
    scope_digest: &str,
) -> Result<(), EnterprisePolicyEvaluationError> {
    transaction
        .execute(
            "INSERT INTO enterprise_policy_evaluation_audit (
                sequence, scope_digest, policy_kind, evaluated_at, input_digest,
                decision_digest, record_json
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                sql_integer(audit.sequence)?,
                scope_digest,
                policy_kind_string(audit.request.input.policy_kind),
                audit.decision.evaluated_at.0,
                audit.decision.input_sha256.0,
                audit.decision.decision_sha256.0,
                canonical_json(audit)?,
            ],
        )
        .map_err(|error| sql_error(&error))?;
    Ok(())
}

fn load_audit(
    connection: &Connection,
    sequence: u64,
) -> Result<EnterprisePolicyEvaluationAudit, EnterprisePolicyEvaluationError> {
    let row = connection
        .query_row(
            "SELECT scope_digest, policy_kind, evaluated_at, input_digest,
                    decision_digest, record_json
             FROM enterprise_policy_evaluation_audit WHERE sequence = ?1",
            [sql_integer(sequence)?],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                ))
            },
        )
        .optional()
        .map_err(|error| sql_error(&error))?
        .ok_or_else(|| corrupt("enterprise Policy evaluation audit is missing"))?;
    let audit: EnterprisePolicyEvaluationAudit = serde_json::from_str(&row.5)
        .map_err(|_| corrupt("enterprise Policy evaluation audit bytes are invalid"))?;
    if canonical_json(&audit)? != row.5
        || audit.sequence != sequence
        || canonical_digest(&audit.request.input.scope)? != row.0
        || policy_kind_string(audit.request.input.policy_kind) != row.1
        || audit.decision.evaluated_at.0 != row.2
        || audit.decision.input_sha256.0 != row.3
        || audit.decision.decision_sha256.0 != row.4
        || evaluation_input_digest(&audit.request.input)? != audit.decision.input_sha256
        || evaluation_decision_digest(&audit.decision)? != audit.decision.decision_sha256
    {
        return Err(corrupt(
            "enterprise Policy evaluation audit differs from canonical bytes",
        ));
    }
    validate_evaluation_command(&EnterprisePolicyEvaluationCommand {
        request: audit.request.clone(),
        actor: audit.actor.clone(),
        request_id: audit.request_id.clone(),
    })
    .map_err(|error| as_corrupt(&error))?;
    Ok(audit)
}

fn insert_receipt(
    transaction: &Transaction<'_>,
    receipt: &ReceiptWrite<'_>,
) -> Result<(), EnterprisePolicyEvaluationError> {
    transaction
        .execute(
            "INSERT INTO enterprise_policy_evaluation_receipts (
                actor_digest, scope_digest, request_id, command_kind, command_digest,
                record_kind, record_id, record_version
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                receipt.actor_digest,
                receipt.scope_digest,
                receipt.request_id.0,
                receipt.command_kind,
                receipt.command_digest,
                receipt.record_kind,
                receipt.record_id,
                sql_integer(receipt.record_version)?
            ],
        )
        .map_err(|error| sql_error(&error))?;
    Ok(())
}

fn load_receipt(
    connection: &Connection,
    actor_digest: &str,
    scope_digest: &str,
    request_id: &RequestId,
) -> Result<Option<StoredReceipt>, EnterprisePolicyEvaluationError> {
    connection
        .query_row(
            "SELECT command_kind, command_digest, record_kind, record_id, record_version
             FROM enterprise_policy_evaluation_receipts
             WHERE actor_digest = ?1 AND scope_digest = ?2 AND request_id = ?3",
            params![actor_digest, scope_digest, request_id.0],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, i64>(4)?,
                ))
            },
        )
        .optional()
        .map_err(|error| sql_error(&error))?
        .map(|row| {
            Ok(StoredReceipt {
                command_kind: row.0,
                command_digest: row.1,
                record_kind: row.2,
                record_id: row.3,
                record_version: from_sql_positive(row.4, "enterprise Policy receipt version")?,
            })
        })
        .transpose()
}

fn replay_evaluation(
    transaction: Transaction<'_>,
    receipt: &StoredReceipt,
    command_digest: &str,
) -> Result<EnterprisePolicyEvaluationReceipt, EnterprisePolicyEvaluationError> {
    if receipt.command_kind != "evaluate"
        || receipt.record_kind != "audit"
        || receipt.command_digest != command_digest
        || receipt.record_id != format!("audit:{}", receipt.record_version)
    {
        return Err(error(
            EnterprisePolicyEvaluationErrorKind::RequestConflict,
            "enterprise Policy evaluation request identity was reused with another command",
        ));
    }
    let audit = load_audit(&transaction, receipt.record_version)?;
    transaction.rollback().map_err(|error| sql_error(&error))?;
    Ok(EnterprisePolicyEvaluationReceipt {
        audit,
        idempotent_replay: true,
    })
}

fn replay_exception(
    transaction: Transaction<'_>,
    receipt: &StoredReceipt,
    command_digest: &str,
    expected_revision: u64,
) -> Result<EnterprisePolicyExceptionReceipt, EnterprisePolicyEvaluationError> {
    if !matches!(
        receipt.command_kind.as_str(),
        "exception_request" | "exception_decide"
    ) || receipt.record_kind != "exception"
        || receipt.command_digest != command_digest
    {
        return Err(error(
            EnterprisePolicyEvaluationErrorKind::RequestConflict,
            "enterprise Policy exception request identity was reused with another command",
        ));
    }
    let exception_id = EnterprisePolicyExceptionId(receipt.record_id.clone());
    let version = load_exception_version(&transaction, &exception_id, receipt.record_version)?;
    let previous_revision = if receipt.command_kind == "exception_request" {
        0
    } else {
        expected_revision
    };
    transaction.rollback().map_err(|error| sql_error(&error))?;
    Ok(EnterprisePolicyExceptionReceipt {
        version,
        previous_revision,
        idempotent_replay: true,
    })
}

fn next_audit_sequence(connection: &Connection) -> Result<u64, EnterprisePolicyEvaluationError> {
    last_audit_sequence(connection)?
        .checked_add(1)
        .filter(|sequence| *sequence <= MAX_SAFE_INTEGER)
        .ok_or_else(|| corrupt("enterprise Policy audit sequence exceeds the safe range"))
}

fn last_audit_sequence(connection: &Connection) -> Result<u64, EnterprisePolicyEvaluationError> {
    let value = connection
        .query_row(
            "SELECT COALESCE(MAX(sequence), 0) FROM enterprise_policy_evaluation_audit",
            [],
            |row| row.get::<_, i64>(0),
        )
        .map_err(|error| sql_error(&error))?;
    from_sql_nonnegative(value, "enterprise Policy audit sequence")
}

fn evaluation_input_digest(
    input: &EnterprisePolicyEvaluationInput,
) -> Result<Sha256Digest, EnterprisePolicyEvaluationError> {
    canonical_sha(&EvaluationTargetFacts {
        scope: &input.scope,
        policy_kind: input.policy_kind,
        resource: &input.resource,
        subject_sha256: &input.subject_sha256,
        matched_condition_sha256: &input.matched_condition_sha256,
    })
}

fn evaluation_command_digest(
    command: &EnterprisePolicyEvaluationCommand,
) -> Result<String, EnterprisePolicyEvaluationError> {
    canonical_digest(&EvaluationCommandFacts {
        input: EvaluationTargetFacts {
            scope: &command.request.input.scope,
            policy_kind: command.request.input.policy_kind,
            resource: &command.request.input.resource,
            subject_sha256: &command.request.input.subject_sha256,
            matched_condition_sha256: &command.request.input.matched_condition_sha256,
        },
        exception_id: &command.request.exception_id,
        actor: &command.actor,
        request_id: &command.request_id,
    })
}

fn exception_request_digest(
    command: &EnterprisePolicyExceptionRequest,
) -> Result<String, EnterprisePolicyEvaluationError> {
    canonical_digest(&ExceptionRequestFacts {
        exception_id: &command.exception_id,
        input: EvaluationTargetFacts {
            scope: &command.input.scope,
            policy_kind: command.input.policy_kind,
            resource: &command.input.resource,
            subject_sha256: &command.input.subject_sha256,
            matched_condition_sha256: &command.input.matched_condition_sha256,
        },
        justification_sha256: &command.justification_sha256,
        expires_at: &command.expires_at,
        actor: &command.actor,
        request_id: &command.request_id,
    })
}

fn exception_decision_digest(
    command: &EnterprisePolicyExceptionDecisionCommand,
) -> Result<String, EnterprisePolicyEvaluationError> {
    canonical_digest(&ExceptionDecisionFacts {
        exception_id: &command.exception_id,
        scope: &command.scope,
        expected_revision: command.expected_revision,
        decision: command.decision,
        actor: &command.actor,
        request_id: &command.request_id,
    })
}

fn exception_record_digest(
    version: &EnterprisePolicyExceptionVersion,
) -> Result<Sha256Digest, EnterprisePolicyEvaluationError> {
    canonical_sha(&ExceptionRecordFacts {
        exception_id: &version.exception_id,
        version: version.version,
        revision: version.revision,
        scope: &version.scope,
        policy_kind: version.policy_kind,
        input_sha256: &version.input_sha256,
        policy_version: &version.policy_version,
        justification_sha256: &version.justification_sha256,
        state: version.state,
        requested_by: &version.requested_by,
        requested_at: &version.requested_at,
        expires_at: &version.expires_at,
        decided_by: &version.decided_by,
        decided_at: &version.decided_at,
        source_request_id: &version.source_request_id,
    })
}

fn evaluation_decision_digest(
    decision: &EnterprisePolicyEvaluation,
) -> Result<Sha256Digest, EnterprisePolicyEvaluationError> {
    canonical_sha(&EvaluationDecisionFacts {
        outcome: decision.outcome,
        reason: decision.reason,
        policy_mode: decision.policy_mode,
        policy_version: &decision.policy_version,
        matched_rule: &decision.matched_rule,
        hard_invariant: decision.hard_invariant,
        exception: &decision.exception,
        input_sha256: &decision.input_sha256,
        evaluated_at: &decision.evaluated_at,
    })
}

fn canonical_digest<T: Serialize>(value: &T) -> Result<String, EnterprisePolicyEvaluationError> {
    Ok(canonical_sha(value)?.0)
}

fn canonical_sha<T: Serialize>(value: &T) -> Result<Sha256Digest, EnterprisePolicyEvaluationError> {
    let canonical = serde_json::to_vec(
        &serde_json::to_value(value)
            .map_err(|_| invalid("enterprise Policy fact is not serializable"))?,
    )
    .map_err(|_| invalid("enterprise Policy fact is not serializable"))?;
    Ok(Sha256Digest(format!(
        "sha256:{:x}",
        Sha256::digest(canonical)
    )))
}

fn canonical_json<T: Serialize>(value: &T) -> Result<String, EnterprisePolicyEvaluationError> {
    serde_json::to_string(value).map_err(|_| invalid("enterprise Policy fact is not serializable"))
}

fn empty_digest() -> Sha256Digest {
    Sha256Digest("sha256:0000000000000000000000000000000000000000000000000000000000000000".into())
}

const fn policy_kind_string(kind: EnterprisePolicyKind) -> &'static str {
    match kind {
        EnterprisePolicyKind::Repository => "repository",
        EnterprisePolicyKind::Model => "model",
        EnterprisePolicyKind::Provider => "provider",
        EnterprisePolicyKind::Tool => "tool",
        EnterprisePolicyKind::Network => "network",
        EnterprisePolicyKind::Approval => "approval",
        EnterprisePolicyKind::Verifier => "verifier",
        EnterprisePolicyKind::WorkerPlacement => "worker_placement",
        EnterprisePolicyKind::Publication => "publication",
        EnterprisePolicyKind::Retention => "retention",
        EnterprisePolicyKind::Integration => "integration",
    }
}

fn sql_integer(value: u64) -> Result<i64, EnterprisePolicyEvaluationError> {
    i64::try_from(value)
        .map_err(|_| invalid("enterprise Policy numeric value exceeds SQLite range"))
}

fn from_sql_positive(value: i64, field: &str) -> Result<u64, EnterprisePolicyEvaluationError> {
    let value = u64::try_from(value).map_err(|_| corrupt(format!("{field} is negative")))?;
    if value == 0 || value > MAX_SAFE_INTEGER {
        Err(corrupt(format!("{field} is outside its canonical range")))
    } else {
        Ok(value)
    }
}

fn from_sql_nonnegative(value: i64, field: &str) -> Result<u64, EnterprisePolicyEvaluationError> {
    let value = u64::try_from(value).map_err(|_| corrupt(format!("{field} is negative")))?;
    if value > MAX_SAFE_INTEGER {
        Err(corrupt(format!("{field} exceeds the safe integer range")))
    } else {
        Ok(value)
    }
}

fn policy_error(error_value: &EnterprisePolicyError) -> EnterprisePolicyEvaluationError {
    let kind = match error_value.kind() {
        EnterprisePolicyErrorKind::InvalidInput => {
            EnterprisePolicyEvaluationErrorKind::InvalidInput
        }
        EnterprisePolicyErrorKind::RevisionConflict => {
            EnterprisePolicyEvaluationErrorKind::RevisionConflict
        }
        EnterprisePolicyErrorKind::RequestConflict => {
            EnterprisePolicyEvaluationErrorKind::RequestConflict
        }
        EnterprisePolicyErrorKind::AuthorityMismatch => {
            EnterprisePolicyEvaluationErrorKind::AuthorityMismatch
        }
        EnterprisePolicyErrorKind::CorruptState => {
            EnterprisePolicyEvaluationErrorKind::CorruptState
        }
        EnterprisePolicyErrorKind::NotFound => EnterprisePolicyEvaluationErrorKind::NotFound,
        EnterprisePolicyErrorKind::Storage => EnterprisePolicyEvaluationErrorKind::Storage,
    };
    error(kind, error_value.to_string())
}

fn storage_error(error_value: &StorageError) -> EnterprisePolicyEvaluationError {
    error(
        EnterprisePolicyEvaluationErrorKind::Storage,
        error_value.to_string(),
    )
}

fn sql_error(error_value: &rusqlite::Error) -> EnterprisePolicyEvaluationError {
    error(
        EnterprisePolicyEvaluationErrorKind::Storage,
        error_value.to_string(),
    )
}

fn as_corrupt(error_value: &EnterprisePolicyEvaluationError) -> EnterprisePolicyEvaluationError {
    corrupt(error_value.to_string())
}

fn invalid(message: impl Into<String>) -> EnterprisePolicyEvaluationError {
    error(EnterprisePolicyEvaluationErrorKind::InvalidInput, message)
}

fn corrupt(message: impl Into<String>) -> EnterprisePolicyEvaluationError {
    error(EnterprisePolicyEvaluationErrorKind::CorruptState, message)
}

fn error(
    kind: EnterprisePolicyEvaluationErrorKind,
    message: impl Into<String>,
) -> EnterprisePolicyEvaluationError {
    EnterprisePolicyEvaluationError {
        kind,
        message: message.into(),
    }
}
