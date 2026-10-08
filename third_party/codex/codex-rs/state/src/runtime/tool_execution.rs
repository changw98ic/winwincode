// SPDX-License-Identifier: Apache-2.0

use super::StateRuntime;
use crate::MAX_TOOL_RESULT_BYTES;
use crate::ToolAttemptClaim;
use crate::ToolAttemptReceipt;
use crate::ToolExecutionFact;
use crate::ToolExecutionStatus;
use crate::ToolFactEvent;
use crate::ToolOutputDelivery;
use crate::ToolOutputDisposition;
use crate::ToolRequestIdentity;
use crate::ToolRequestObservation;
use crate::ToolStoredResults;
use anyhow::ensure;
use sqlx::QueryBuilder;
use sqlx::Row;
use sqlx::Sqlite;
use sqlx::SqliteConnection;

const FACT_SELECT: &str =
    "SELECT r.sequence, r.request_json, r.resolution, a.attempt_id, a.owner_id,
 a.operation_digest, a.execution, a.disposition, a.delivery, a.revision,
 a.execution_result IS NOT NULL AS execution_result_retained,
 a.accepted_result IS NOT NULL AS accepted_result_retained
 FROM tool_requests r LEFT JOIN tool_attempts a ON a.request_sequence = r.sequence";

impl StateRuntime {
    /// The latest four payload-free calls in one exact Code Mode cell scope.
    pub async fn list_tool_cell_receipts(
        &self,
        thread: &str,
        cell: &str,
        scope: &str,
    ) -> anyhow::Result<Vec<ToolExecutionFact>> {
        let mut query = QueryBuilder::<Sqlite>::new(FACT_SELECT);
        query
            .push(" WHERE r.thread_id=")
            .push_bind(thread)
            .push(" AND json_extract(r.request_json,'$.cell_id')=")
            .push_bind(cell)
            .push(" AND json_extract(r.request_json,'$.scope_id')=")
            .push_bind(scope)
            .push(" ORDER BY r.sequence DESC LIMIT 4");
        let rows = query.build().fetch_all(self.pool.as_ref()).await?;
        rows.iter().map(decode_fact).collect()
    }

    /// Creates one observation, atomically distinguishing transport replays and conflicts.
    pub async fn observe_tool_request(
        &self,
        request: &ToolRequestIdentity,
    ) -> anyhow::Result<ToolRequestObservation> {
        ensure!(
            !request.thread_id.is_empty() && !request.logical_id.is_empty(),
            "missing tool request identity"
        );
        let encoded = serde_json::to_string(request)?;
        ensure!(
            encoded.len() <= 16 * 1024,
            "tool request metadata exceeds limit"
        );
        let mut tx = self.pool.begin().await?;
        let inserted = sqlx::query(
            "INSERT INTO tool_requests(thread_id, logical_id, binding, request_json)
            VALUES (?, ?, ?, ?) ON CONFLICT(thread_id, logical_id) DO NOTHING",
        )
        .bind(&request.thread_id)
        .bind(&request.logical_id)
        .bind(&request.binding)
        .bind(encoded)
        .execute(&mut *tx)
        .await?
        .rows_affected()
            == 1;
        let mut query = QueryBuilder::<Sqlite>::new(FACT_SELECT);
        query
            .push(" WHERE r.thread_id = ")
            .push_bind(&request.thread_id)
            .push(" AND r.logical_id = ")
            .push_bind(&request.logical_id);
        let row = query.build().fetch_one(&mut *tx).await?;
        let fact = decode_fact(&row)?;
        let outcome = if inserted {
            append_event(&mut tx, &fact).await?;
            ToolRequestObservation::New(fact)
        } else if fact.request.binding != request.binding
            || fact.request.tool_name != request.tool_name
            || fact.request.scope_id != request.scope_id
            || fact.request.cell_id != request.cell_id
            || fact.request.parent_call_id != request.parent_call_id
            || fact.request.source != request.source
        {
            ToolRequestObservation::IdentityConflict
        } else {
            ToolRequestObservation::Replay(fact)
        };
        tx.commit().await?;
        Ok(outcome)
    }

    /// Reads an accepted body only for the currently authorized receipt revision.
    /// The caller must authorize this exact receipt before invoking this method.
    pub async fn read_accepted_tool_output(
        &self,
        request_sequence: i64,
        attempt_id: &str,
        revision: i64,
    ) -> anyhow::Result<Option<ToolStoredResults>> {
        let row = sqlx::query("SELECT effective_input, accepted_result FROM tool_attempts WHERE request_sequence = ? AND attempt_id = ? AND revision = ? AND execution = 'completed' AND disposition = 'accepted' AND accepted_result IS NOT NULL")
            .bind(request_sequence).bind(attempt_id).bind(revision)
            .fetch_optional(self.pool.as_ref()).await?;
        row.map(|row| {
            Ok(ToolStoredResults {
                effective_input: row.try_get("effective_input")?,
                execution: None,
                accepted: row.try_get("accepted_result")?,
                rejection: None,
            })
        })
        .transpose()
    }

    /// Read-only current metadata for an original actual execution.
    pub async fn tool_execution_fact(&self, sequence: i64) -> anyhow::Result<ToolExecutionFact> {
        let mut connection = self.pool.acquire().await?;
        load_fact(&mut connection, sequence).await
    }

    /// Reads receipt metadata by its original logical identity.
    pub async fn tool_request_fact(
        &self,
        thread_id: &str,
        logical_id: &str,
    ) -> anyhow::Result<Option<ToolExecutionFact>> {
        let mut query = QueryBuilder::<Sqlite>::new(FACT_SELECT);
        query
            .push(" WHERE r.thread_id = ")
            .push_bind(thread_id)
            .push(" AND r.logical_id = ")
            .push_bind(logical_id);
        query
            .build()
            .fetch_optional(self.pool.as_ref())
            .await?
            .as_ref()
            .map(decode_fact)
            .transpose()
    }

    /// A pre-dispatch rejection remains distinct from an executed handler.
    pub async fn deny_tool_request(&self, request_sequence: i64) -> anyhow::Result<()> {
        let mut tx = self.pool.begin().await?;
        let changed = sqlx::query("UPDATE tool_requests SET resolution = 'denied' WHERE sequence = ? AND resolution = 'observed' AND NOT EXISTS(SELECT 1 FROM tool_attempts WHERE request_sequence = ?)")
            .bind(request_sequence).bind(request_sequence).execute(&mut *tx).await?.rows_affected();
        ensure!(
            changed == 1,
            "tool request has already been dispatched or denied"
        );
        let fact = load_fact(&mut tx, request_sequence).await?;
        append_event(&mut tx, &fact).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Compare-and-set before dispatch. Existing claims are never silently retried.
    pub async fn claim_tool_attempt(
        &self,
        request_sequence: i64,
        attempt_id: &str,
        owner_id: &str,
        operation_digest: &str,
        effective_input: &str,
    ) -> anyhow::Result<ToolAttemptClaim> {
        ensure!(
            effective_input.len() <= MAX_TOOL_RESULT_BYTES,
            "effective tool input exceeds retention limit"
        );
        let mut tx = self.pool.begin().await?;
        let inserted = sqlx::query("INSERT INTO tool_attempts(request_sequence, attempt_id, owner_id, operation_digest, effective_input, execution)
            SELECT ?, ?, ?, ?, ?, 'running' WHERE EXISTS(SELECT 1 FROM tool_requests WHERE sequence = ? AND resolution = 'observed' AND NOT EXISTS(SELECT 1 FROM tool_sharing s WHERE s.request_sequence=tool_requests.sequence)) ON CONFLICT(request_sequence) DO NOTHING")
            .bind(request_sequence).bind(attempt_id).bind(owner_id).bind(operation_digest).bind(effective_input).bind(request_sequence)
            .execute(&mut *tx).await?.rows_affected() == 1;
        let fact = load_fact(&mut tx, request_sequence).await?;
        let outcome = if inserted {
            append_event(&mut tx, &fact).await?;
            ToolAttemptClaim::Claimed(fact)
        } else {
            ToolAttemptClaim::Existing(fact)
        };
        tx.commit().await?;
        Ok(outcome)
    }

    /// Commit actual execution before running output hooks. Oversize bodies are not retained.
    pub async fn complete_tool_attempt(
        &self,
        request_sequence: i64,
        attempt_id: &str,
        owner_id: &str,
        execution: ToolExecutionStatus,
        result: Option<&str>,
    ) -> anyhow::Result<()> {
        ensure!(
            execution != ToolExecutionStatus::Running,
            "execution is still running"
        );
        let state = match execution {
            ToolExecutionStatus::Completed => "completed",
            ToolExecutionStatus::Uncertain => "uncertain",
            ToolExecutionStatus::Running => unreachable!(),
        };
        let body = result.filter(|body| body.len() <= MAX_TOOL_RESULT_BYTES);
        let mut tx = self.pool.begin().await?;
        let changed = sqlx::query("UPDATE tool_attempts SET execution = ?, execution_result = ?, revision = revision + 1
            WHERE request_sequence = ? AND attempt_id = ? AND owner_id = ? AND execution = 'running'")
            .bind(state).bind(body).bind(request_sequence).bind(attempt_id).bind(owner_id)
            .execute(&mut *tx).await?.rows_affected();
        ensure!(changed == 1, "tool execution claim is no longer owned");
        let fact = load_fact(&mut tx, request_sequence).await?;
        append_event(&mut tx, &fact).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Output acceptance is independent of successful execution and delivery.
    pub async fn decide_tool_output(
        &self,
        request_sequence: i64,
        attempt_id: &str,
        owner_id: &str,
        disposition: ToolOutputDisposition,
        result: Option<&str>,
    ) -> anyhow::Result<()> {
        ensure!(
            disposition != ToolOutputDisposition::Pending,
            "output decision is pending"
        );
        let state = match disposition {
            ToolOutputDisposition::Accepted => "accepted",
            ToolOutputDisposition::Rejected => "rejected",
            ToolOutputDisposition::Pending => unreachable!(),
        };
        let body = result.filter(|body| body.len() <= MAX_TOOL_RESULT_BYTES);
        let mut tx = self.pool.begin().await?;
        let changed = sqlx::query("UPDATE tool_attempts SET disposition = ?, accepted_result = ?, rejection = ?, revision = revision + 1
            WHERE request_sequence = ? AND attempt_id = ? AND owner_id = ? AND (execution = 'completed' OR ? = 'rejected') AND execution != 'running' AND disposition = 'pending'")
            .bind(state).bind(if disposition == ToolOutputDisposition::Accepted { body } else { None })
            .bind(if disposition == ToolOutputDisposition::Rejected { body } else { None })
            .bind(request_sequence).bind(attempt_id).bind(owner_id).bind(state)
            .execute(&mut *tx).await?.rows_affected();
        ensure!(
            changed == 1,
            "tool output decision conflicts with its receipt"
        );
        let fact = load_fact(&mut tx, request_sequence).await?;
        append_event(&mut tx, &fact).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Marks only an offer to the caller. Transport acknowledgement is a separate fact.
    pub async fn offer_tool_output(
        &self,
        request_sequence: i64,
        attempt_id: &str,
        owner_id: &str,
    ) -> anyhow::Result<()> {
        let mut tx = self.pool.begin().await?;
        let changed = sqlx::query("UPDATE tool_attempts SET delivery = 'offered', revision = revision + 1
            WHERE request_sequence = ? AND attempt_id = ? AND owner_id = ? AND disposition = 'accepted' AND delivery = 'pending'")
            .bind(request_sequence).bind(attempt_id).bind(owner_id).execute(&mut *tx).await?.rows_affected();
        if changed == 1 {
            let fact = load_fact(&mut tx, request_sequence).await?;
            append_event(&mut tx, &fact).await?;
        } else {
            let fact = load_fact(&mut tx, request_sequence).await?;
            ensure!(
                fact.attempt.is_some_and(|a| a.attempt_id == attempt_id
                    && a.owner_id == owner_id
                    && a.delivery == ToolOutputDelivery::Offered),
                "tool output has not been accepted"
            );
        }
        tx.commit().await?;
        if let Err(error) = self.offer_staged_diagnostics(request_sequence).await {
            tracing::warn!(request_sequence, %error,
                "failed to record optional tool diagnosis offer");
        }
        Ok(())
    }

    /// Private payload read. Callers must verify current read authority and result validity first.
    pub async fn read_tool_results(
        &self,
        request_sequence: i64,
    ) -> anyhow::Result<ToolStoredResults> {
        let row = sqlx::query("SELECT effective_input, execution_result, accepted_result, rejection FROM tool_attempts WHERE request_sequence = ?")
            .bind(request_sequence).fetch_one(self.pool.as_ref()).await?;
        Ok(ToolStoredResults {
            effective_input: row.try_get("effective_input")?,
            execution: row.try_get("execution_result")?,
            accepted: row.try_get("accepted_result")?,
            rejection: row.try_get("rejection")?,
        })
    }

    /// Result cleanup preserves execution receipts and projection history.
    pub async fn forget_tool_results(&self, request_sequence: i64) -> anyhow::Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("UPDATE tool_attempts SET execution_result = NULL, accepted_result = NULL, rejection = NULL, revision = revision + 1 WHERE request_sequence = ?")
            .bind(request_sequence).execute(&mut *tx).await?;
        let fact = load_fact(&mut tx, request_sequence).await?;
        append_event(&mut tx, &fact).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Bounded versioned events contain receipt metadata only, never result bodies.
    pub async fn list_tool_fact_events(
        &self,
        thread_id: &str,
        after: i64,
        limit: u32,
    ) -> anyhow::Result<Vec<ToolFactEvent>> {
        let rows = sqlx::query("SELECT sequence, json_extract(fact_json, '$.fact') AS fact_json FROM tool_fact_events WHERE thread_id = ? AND sequence > ? AND json_extract(fact_json, '$.kind') = 'request' ORDER BY sequence LIMIT ?")
            .bind(thread_id).bind(after).bind(limit.clamp(1, 200)).fetch_all(self.pool.as_ref()).await?;
        rows.into_iter()
            .map(|row| {
                Ok(ToolFactEvent {
                    sequence: row.try_get("sequence")?,
                    fact: serde_json::from_str(row.try_get("fact_json")?)?,
                })
            })
            .collect()
    }
}

async fn load_fact(
    connection: &mut SqliteConnection,
    sequence: i64,
) -> anyhow::Result<ToolExecutionFact> {
    let mut query = QueryBuilder::<Sqlite>::new(FACT_SELECT);
    query.push(" WHERE r.sequence = ").push_bind(sequence);
    decode_fact(&query.build().fetch_one(connection).await?)
}

async fn append_event(
    connection: &mut SqliteConnection,
    fact: &ToolExecutionFact,
) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO tool_fact_events(request_sequence, thread_id, fact_json) VALUES (?, ?, ?)",
    )
    .bind(fact.request_sequence)
    .bind(&fact.request.thread_id)
    .bind(serde_json::to_string(&crate::ToolRuntimeFact::Request(
        fact.clone(),
    ))?)
    .execute(connection)
    .await?;
    Ok(())
}

fn decode_fact(row: &sqlx::sqlite::SqliteRow) -> anyhow::Result<ToolExecutionFact> {
    let attempt = if let Some(attempt_id) = row.try_get::<Option<String>, _>("attempt_id")? {
        let execution: &str = row.try_get("execution")?;
        let disposition: &str = row.try_get("disposition")?;
        let delivery: &str = row.try_get("delivery")?;
        Some(ToolAttemptReceipt {
            attempt_id,
            owner_id: row.try_get("owner_id")?,
            operation_digest: row.try_get("operation_digest")?,
            execution: serde_json::from_value(serde_json::json!(execution))?,
            disposition: serde_json::from_value(serde_json::json!(disposition))?,
            delivery: serde_json::from_value(serde_json::json!(delivery))?,
            revision: row.try_get("revision")?,
            execution_result_retained: row.try_get("execution_result_retained")?,
            accepted_result_retained: row.try_get("accepted_result_retained")?,
        })
    } else {
        None
    };
    Ok(ToolExecutionFact {
        schema_version: 1,
        request_sequence: row.try_get("sequence")?,
        request: serde_json::from_str(row.try_get("request_json")?)?,
        resolution: serde_json::from_value(serde_json::json!(
            row.try_get::<&str, _>("resolution")?
        ))?,
        attempt,
    })
}

#[cfg(test)]
#[path = "tool_execution_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "tool_execution_migration_tests.rs"]
mod migration_tests;

#[cfg(test)]
#[path = "tool_receipts_tests.rs"]
mod receipt_tests;
