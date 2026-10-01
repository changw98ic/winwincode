// SPDX-License-Identifier: Apache-2.0

//! Append-only Provider receipts and attempt manifests. Execution outcomes and
//! the original unknown-accounting markers are never rewritten or deleted.

use crate::{ExecutionLeaseRecord, SqliteStorage, StorageError};
use rusqlite::{OptionalExtension, params};
use sha2::{Digest, Sha256};
use winwincode_domain::{ExecutionJobId, Instant, RequestId};
use winwincode_execution_port::{
    accounting::ProviderAccountingReceipt,
    generated::{
        ExecutionLeaseStamp, ExecutionOutcomeUsage, ExecutionOutcomeUsageAccountingStatus,
    },
};

pub(crate) const SCHEMA: &str = r"
CREATE TABLE IF NOT EXISTS execution_attempt_accounting_manifests (
 job_id TEXT NOT NULL, attempt INTEGER NOT NULL, lease_json TEXT NOT NULL,
 manifest_json TEXT NOT NULL, PRIMARY KEY(job_id,attempt)
);
CREATE TABLE IF NOT EXISTS execution_attempt_provider_receipts (
 job_id TEXT NOT NULL, attempt INTEGER NOT NULL, slot TEXT NOT NULL,
 provider_id TEXT NOT NULL, provider_receipt_id TEXT NOT NULL, receipt_json TEXT NOT NULL,
 PRIMARY KEY(job_id,attempt,slot), UNIQUE(provider_id,provider_receipt_id)
);
CREATE TABLE IF NOT EXISTS execution_attempt_accounting_facts (
 statement_digest TEXT PRIMARY KEY NOT NULL, job_id TEXT NOT NULL,
 attempt INTEGER NOT NULL, statement_json TEXT NOT NULL, observed_at TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS execution_attempt_accounting_totals (
 job_id TEXT NOT NULL, attempt INTEGER NOT NULL, tokens INTEGER, cost INTEGER, known_tokens INTEGER NOT NULL,
 PRIMARY KEY(job_id,attempt)
);
CREATE TABLE IF NOT EXISTS execution_attempt_terminal_usage (
 job_id TEXT NOT NULL, attempt INTEGER NOT NULL, usage_json TEXT NOT NULL,
 PRIMARY KEY(job_id,attempt)
);
";

fn invalid() -> StorageError {
    StorageError::invalid_input("Provider attempt accounting conflicts with durable authority")
}
fn adapter(error: impl std::fmt::Display) -> StorageError {
    StorageError::adapter(error.to_string())
}

pub(crate) fn same_lease(stamp: &ExecutionLeaseStamp, lease: &ExecutionLeaseRecord) -> bool {
    stamp.job_id == lease.job_id
        && stamp.lease_id == lease.lease_id
        && stamp.worker_id == lease.worker_id
        && stamp.worker_instance_id == lease.worker_instance_id
        && u64::try_from(stamp.attempt).ok() == Some(lease.attempt)
        && stamp.fencing_token == lease.fencing_token
        && stamp.issued_at == lease.issued_at
}

impl SqliteStorage {
    /// Lists bounded financially unresolved attempts for the trusted Device reconciler.
    /// # Errors
    /// Rejects corrupt leases or unavailable storage.
    pub fn pending_accounting_leases(
        &mut self,
        offset: u64,
    ) -> Result<Vec<ExecutionLeaseStamp>, StorageError> {
        self.execution_admission().map_err(adapter)?;
        let offset = i64::try_from(offset).map_err(|_| invalid())?;
        let jobs = {
            let mut query = self.connection()?.prepare("SELECT job_id,lease_json FROM execution_attempt_accounting_pending p WHERE NOT EXISTS(SELECT 1 FROM execution_attempt_accounting_totals t WHERE t.job_id=p.job_id AND t.attempt=p.attempt AND t.tokens IS NOT NULL AND t.cost IS NOT NULL) UNION ALL SELECT job_id,NULL FROM execution_admission_reconciliation WHERE accounting_status='pending' ORDER BY job_id,lease_json LIMIT 100 OFFSET ?1") .map_err(adapter)?;
            query
                .query_map([offset], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
                })
                .map_err(adapter)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(adapter)?
        };
        let mut leases = Vec::new();
        for (job, previous) in jobs {
            let lease = match previous {
                Some(previous) => serde_json::from_str(&previous).map_err(adapter)?,
                None => self
                    .execution_registry()?
                    .load_lease(&ExecutionJobId(job))?
                    .ok_or_else(invalid)?,
            };
            leases.push(ExecutionLeaseStamp {
                job_id: lease.job_id,
                lease_id: lease.lease_id,
                worker_id: lease.worker_id,
                worker_instance_id: lease.worker_instance_id,
                attempt: i64::try_from(lease.attempt).map_err(adapter)?,
                fencing_token: lease.fencing_token,
                issued_at: lease.issued_at,
                expires_at: lease.expires_at,
            });
        }
        Ok(leases)
    }

    /// Retains authenticated current-attempt usage before combining predecessors.
    /// # Errors
    /// Rejects conflicting replays or unavailable storage.
    pub fn combine_execution_attempt_usage(
        &mut self,
        lease: &ExecutionLeaseStamp,
        usage: &ExecutionOutcomeUsage,
    ) -> Result<ExecutionOutcomeUsage, StorageError> {
        self.execution_admission().map_err(adapter)?;
        if !winwincode_execution_port::usage::valid_usage(usage) {
            return Err(invalid());
        }
        let current = self
            .execution_registry()?
            .load_lease(&lease.job_id)?
            .ok_or_else(invalid)?;
        if !same_lease(lease, &current) {
            return Err(invalid());
        }
        let value = serde_json::to_string(usage).map_err(adapter)?;
        let previous: Option<String> = self.connection()?.query_row("SELECT usage_json FROM execution_attempt_terminal_usage WHERE job_id=?1 AND attempt=?2",params![lease.job_id.0,lease.attempt],|row|row.get(0)).optional().map_err(adapter)?;
        if previous.as_ref().is_some_and(|previous| previous != &value) {
            return Err(invalid());
        }
        self.connection()?
            .execute(
                "INSERT OR IGNORE INTO execution_attempt_terminal_usage VALUES(?1,?2,?3)",
                params![lease.job_id.0, lease.attempt, value],
            )
            .map_err(adapter)?;
        let mut combined = usage.clone();
        let mut query = self.connection()?.prepare("SELECT t.tokens,t.cost,COALESCE(t.known_tokens,0) FROM execution_attempt_accounting_pending p LEFT JOIN execution_attempt_accounting_totals t ON t.job_id=p.job_id AND t.attempt=p.attempt WHERE p.job_id=?1").map_err(adapter)?;
        let rows = query
            .query_map([&lease.job_id.0], |row| {
                Ok((
                    row.get::<_, Option<i64>>(0)?,
                    row.get::<_, Option<i64>>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })
            .map_err(adapter)?;
        for row in rows {
            let (tokens, cost, lower) = row.map_err(adapter)?;
            add_attempt(&mut combined, tokens, cost)?;
            if tokens.is_none() {
                add_lower(&mut combined, lower)?;
            }
        }
        Ok(combined)
    }

    /// Imports a signature-verified statement from the trusted Provider adapter.
    /// Signature checking belongs to the production boundary, never to a Worker outcome.
    /// # Errors
    /// Rejects foreign leases, changed manifests, reused Provider receipts, or changed known usage.
    #[allow(clippy::too_many_lines)]
    pub fn reconcile_provider_attempt(
        &mut self,
        verified: &winwincode_execution_port::accounting::VerifiedAccountingStatement,
        now: &Instant,
    ) -> Result<(), StorageError> {
        self.execution_admission().map_err(adapter)?;
        let statement = verified.statement();
        let bytes = statement.encode().map_err(adapter)?;
        let digest = format!("sha256:{:x}", Sha256::digest(&bytes));
        let pending: Option<String> = self.connection()?.query_row("SELECT lease_json FROM execution_attempt_accounting_pending WHERE job_id=?1 AND attempt=?2",params![statement.lease.job_id.0,statement.lease.attempt],|row|row.get(0)).optional().map_err(adapter)?;
        let lease = if let Some(value) = pending {
            serde_json::from_str(&value).map_err(adapter)?
        } else {
            let reservation = self
                .execution_admission()
                .map_err(adapter)?
                .load_reservation_by_job(&statement.lease.job_id)
                .map_err(adapter)?
                .ok_or_else(invalid)?;
            if !matches!(
                reservation.state,
                crate::ExecutionReservationState::Released
                    | crate::ExecutionReservationState::Settled
            ) {
                return Err(invalid());
            }
            self.execution_registry()?
                .load_lease(&statement.lease.job_id)?
                .ok_or_else(invalid)?
        };
        if !same_lease(&statement.lease, &lease) {
            return Err(invalid());
        }
        let tx = self
            .connection_mut()?
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(adapter)?;
        let replay: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM execution_attempt_accounting_facts WHERE statement_digest=?1)",[&digest],|row|row.get(0)).map_err(adapter)?;
        if !replay {
            let manifest = serde_json::to_string(&statement.manifest).map_err(adapter)?;
            let previous: Option<String> = tx.query_row("SELECT manifest_json FROM execution_attempt_accounting_manifests WHERE job_id=?1 AND attempt=?2",params![lease.job_id.0,statement.lease.attempt],|row|row.get(0)).optional().map_err(adapter)?;
            if previous.as_ref().is_some_and(|value| value != &manifest) {
                return Err(invalid());
            }
            tx.execute(
                "INSERT OR IGNORE INTO execution_attempt_accounting_manifests VALUES(?1,?2,?3,?4)",
                params![
                    lease.job_id.0,
                    statement.lease.attempt,
                    serde_json::to_string(&lease).map_err(adapter)?,
                    manifest
                ],
            )
            .map_err(adapter)?;
            for receipt in &statement.receipts {
                let owner: Option<(String, i64, String)> = tx.query_row(
                    "SELECT job_id,attempt,slot FROM execution_attempt_provider_receipts WHERE provider_id=?1 AND provider_receipt_id=?2",
                    params![receipt.provider_id, receipt.provider_receipt_id],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                ).optional().map_err(adapter)?;
                if owner.is_some_and(|owner| {
                    owner
                        != (
                            lease.job_id.0.clone(),
                            statement.lease.attempt,
                            receipt.slot.clone(),
                        )
                }) {
                    return Err(invalid());
                }
                let previous: Option<String> = tx.query_row("SELECT receipt_json FROM execution_attempt_provider_receipts WHERE job_id=?1 AND attempt=?2 AND slot=?3",params![lease.job_id.0,statement.lease.attempt,receipt.slot],|row|row.get(0)).optional().map_err(adapter)?;
                if let Some(previous) = previous {
                    let previous: ProviderAccountingReceipt =
                        serde_json::from_str(&previous).map_err(adapter)?;
                    if previous.model_exchange_id != receipt.model_exchange_id
                        || previous.provider_id != receipt.provider_id
                        || previous.provider_receipt_id != receipt.provider_receipt_id
                        || previous
                            .tokens
                            .is_some_and(|value| receipt.tokens != Some(value))
                        || previous
                            .cost_microunits
                            .is_some_and(|value| receipt.cost_microunits != Some(value))
                        || (previous.tokens == receipt.tokens
                            && previous.cost_microunits == receipt.cost_microunits
                            && previous.source_digest != receipt.source_digest)
                    {
                        return Err(invalid());
                    }
                }
                tx.execute("INSERT INTO execution_attempt_provider_receipts VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(job_id,attempt,slot) DO UPDATE SET receipt_json=excluded.receipt_json",params![lease.job_id.0,statement.lease.attempt,receipt.slot,receipt.provider_id,receipt.provider_receipt_id,serde_json::to_string(receipt).map_err(adapter)?]).map_err(adapter)?;
            }
            let mut totals = known_usage(0);
            for slot in &statement.manifest {
                let receipt: Option<String> = tx.query_row("SELECT receipt_json FROM execution_attempt_provider_receipts WHERE job_id=?1 AND attempt=?2 AND slot=?3",params![lease.job_id.0,statement.lease.attempt,slot],|row|row.get(0)).optional().map_err(adapter)?;
                let receipt: Option<ProviderAccountingReceipt> = receipt
                    .map(|value| serde_json::from_str(&value))
                    .transpose()
                    .map_err(adapter)?;
                add_attempt(
                    &mut totals,
                    receipt
                        .as_ref()
                        .and_then(|receipt| receipt.tokens)
                        .map(i64::try_from)
                        .transpose()
                        .map_err(adapter)?,
                    receipt
                        .as_ref()
                        .and_then(|receipt| receipt.cost_microunits)
                        .map(i64::try_from)
                        .transpose()
                        .map_err(adapter)?,
                )?;
            }
            merge_terminal_metrics(&tx, statement, &mut totals)?;
            tx.execute("INSERT INTO execution_attempt_accounting_totals VALUES(?1,?2,?3,?4,?5) ON CONFLICT(job_id,attempt) DO UPDATE SET tokens=excluded.tokens,cost=excluded.cost,known_tokens=excluded.known_tokens",params![lease.job_id.0,statement.lease.attempt,totals.tokens,totals.cost_microunits,totals.known_tokens]).map_err(adapter)?;
            tx.execute(
                "INSERT INTO execution_attempt_accounting_facts VALUES(?1,?2,?3,?4,?5)",
                params![
                    digest,
                    lease.job_id.0,
                    statement.lease.attempt,
                    String::from_utf8(bytes).map_err(adapter)?,
                    now.0
                ],
            )
            .map_err(adapter)?;
        }
        tx.commit().map_err(adapter)?;
        // Import and projection are independently replayable. A crash between them
        // leaves source facts intact; retrying the same receipt repairs the projection.
        self.reconcile_attempt_totals(&lease.job_id, now)
    }

    fn reconcile_attempt_totals(
        &mut self,
        job: &ExecutionJobId,
        now: &Instant,
    ) -> Result<(), StorageError> {
        let terminal: Option<(String,String)> = self.connection()?.query_row("SELECT terminal_request_id,terminal_usage_json FROM execution_admission_reconciliation WHERE job_id=?1",[&job.0],|row|Ok((row.get(0)?,row.get(1)?))).optional().map_err(adapter)?;
        let Some((terminal_id, original)) = terminal else {
            return Ok(());
        };
        let original: ExecutionOutcomeUsage = serde_json::from_str(&original).map_err(adapter)?;
        let current = self
            .execution_registry()?
            .load_lease(job)?
            .ok_or_else(invalid)?;
        let mut usage = known_usage(original.runtime_millis);
        let mut query = self.connection()?.prepare("SELECT attempt,tokens,cost,known_tokens FROM execution_attempt_accounting_totals WHERE job_id=?1 UNION ALL SELECT p.attempt,NULL,NULL,0 FROM execution_attempt_accounting_pending p WHERE job_id=?1 AND NOT EXISTS(SELECT 1 FROM execution_attempt_accounting_totals t WHERE t.job_id=p.job_id AND t.attempt=p.attempt)").map_err(adapter)?;
        let rows = query
            .query_map([&job.0], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, Option<i64>>(1)?,
                    row.get::<_, Option<i64>>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            })
            .map_err(adapter)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(adapter)?;
        drop(query);
        let has_current = rows
            .iter()
            .any(|(attempt, _, _, _)| u64::try_from(*attempt).ok() == Some(current.attempt));
        for (_, tokens, cost, lower) in rows {
            add_attempt(&mut usage, tokens, cost)?;
            if tokens.is_none() {
                add_lower(&mut usage, lower)?;
            }
        }
        if !has_current {
            let current_usage:Option<String> = self.connection()?.query_row("SELECT usage_json FROM execution_attempt_terminal_usage WHERE job_id=?1 AND attempt=?2",params![job.0,i64::try_from(current.attempt).map_err(adapter)?],|row|row.get(0)).optional().map_err(adapter)?;
            let current_usage: Option<ExecutionOutcomeUsage> = current_usage
                .map(|value| serde_json::from_str(&value))
                .transpose()
                .map_err(adapter)?;
            add_attempt(
                &mut usage,
                current_usage.as_ref().and_then(|value| value.tokens),
                current_usage
                    .as_ref()
                    .and_then(|value| value.cost_microunits),
            )?;
            if current_usage
                .as_ref()
                .is_some_and(|value| value.tokens.is_none())
            {
                add_lower(
                    &mut usage,
                    current_usage.as_ref().map_or(0, |value| value.known_tokens),
                )?;
            }
        }
        usage.known_tokens = usage.known_tokens.max(original.known_tokens);
        if usage
            .tokens
            .is_some_and(|tokens| tokens < usage.known_tokens)
        {
            return Err(invalid());
        }
        let bytes = serde_json::to_vec(&(job, &terminal_id, &usage)).map_err(adapter)?;
        let suffix = format!("{:X}", Sha256::digest(bytes));
        let request = RequestId(format!("req_{}", &suffix[..26]));
        let observed: Option<String> = self.connection()?.query_row("SELECT observed_at FROM execution_admission_reconciliation_facts WHERE settlement_request_id=?1",[&request.0],|row|row.get(0)).optional().map_err(adapter)?;
        let observed = observed.map_or_else(|| now.clone(), Instant);
        self.execution_admission()
            .map_err(adapter)?
            .settle_reconciliation(job, &RequestId(terminal_id), &request, &usage, &observed)
            .map_err(adapter)?;
        Ok(())
    }
}

fn add_attempt(
    usage: &mut ExecutionOutcomeUsage,
    tokens: Option<i64>,
    cost: Option<i64>,
) -> Result<(), StorageError> {
    if let Some(tokens) = tokens {
        usage.known_tokens = usage
            .known_tokens
            .checked_add(tokens)
            .filter(|value| *value <= 9_007_199_254_740_991)
            .ok_or_else(invalid)?;
    }
    usage.tokens = usage
        .tokens
        .zip(tokens)
        .map(|(left, right)| left.checked_add(right).ok_or_else(invalid))
        .transpose()?;
    usage.cost_microunits = usage
        .cost_microunits
        .zip(cost)
        .map(|(left, right)| {
            left.checked_add(right)
                .filter(|value| *value <= 9_007_199_254_740_991)
                .ok_or_else(invalid)
        })
        .transpose()?;
    usage.accounting_status = if usage.tokens.is_some() {
        ExecutionOutcomeUsageAccountingStatus::Known
    } else {
        ExecutionOutcomeUsageAccountingStatus::Unknown
    };
    Ok(())
}

fn known_usage(runtime_millis: i64) -> ExecutionOutcomeUsage {
    ExecutionOutcomeUsage {
        runtime_millis,
        tokens: Some(0),
        known_tokens: 0,
        cost_microunits: Some(0),
        accounting_status: ExecutionOutcomeUsageAccountingStatus::Known,
    }
}

fn merge_terminal_metrics(
    connection: &rusqlite::Connection,
    statement: &winwincode_execution_port::accounting::AttemptAccountingStatement,
    totals: &mut ExecutionOutcomeUsage,
) -> Result<(), StorageError> {
    let terminal: Option<String> = connection.query_row("SELECT usage_json FROM execution_attempt_terminal_usage WHERE job_id=?1 AND attempt=?2",params![statement.lease.job_id.0,statement.lease.attempt],|row|row.get(0)).optional().map_err(adapter)?;
    // Upgrade compatibility: older terminals did not retain an attempt row.
    // Their job usage can identify the current attempt only without predecessors.
    let terminal = if terminal.is_some() {
        terminal
    } else {
        connection.query_row("SELECT terminal_usage_json FROM execution_admission_reconciliation WHERE job_id=?1 AND NOT EXISTS(SELECT 1 FROM execution_attempt_accounting_pending WHERE job_id=?1)",[&statement.lease.job_id.0],|row|row.get(0)).optional().map_err(adapter)?
    };
    if let Some(terminal) = terminal {
        let terminal: ExecutionOutcomeUsage = serde_json::from_str(&terminal).map_err(adapter)?;
        if totals
            .tokens
            .zip(terminal.tokens)
            .is_some_and(|(a, b)| a != b)
            || totals
                .cost_microunits
                .zip(terminal.cost_microunits)
                .is_some_and(|(a, b)| a != b)
        {
            return Err(invalid());
        }
        totals.tokens = totals.tokens.or(terminal.tokens);
        totals.cost_microunits = totals.cost_microunits.or(terminal.cost_microunits);
        totals.known_tokens = totals.known_tokens.max(terminal.known_tokens);
        totals.accounting_status = if totals.tokens.is_some() {
            ExecutionOutcomeUsageAccountingStatus::Known
        } else {
            ExecutionOutcomeUsageAccountingStatus::Unknown
        };
        if !winwincode_execution_port::usage::valid_usage(totals) {
            return Err(invalid());
        }
    }
    Ok(())
}

fn add_lower(usage: &mut ExecutionOutcomeUsage, lower: i64) -> Result<(), StorageError> {
    usage.known_tokens = usage
        .known_tokens
        .checked_add(lower)
        .filter(|value| *value <= 9_007_199_254_740_991 && *value >= 0)
        .ok_or_else(invalid)?;
    Ok(())
}
