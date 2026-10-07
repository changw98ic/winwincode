// SPDX-License-Identifier: Apache-2.0
use super::StateRuntime;
use crate::ToolReconciliationFact;
use crate::ToolRuntimeFact;
use anyhow::ensure;

impl StateRuntime {
    /// Appends changed downstream evidence for an exact original receipt revision.
    /// The caller must authorize and revalidate its read before committing this fact.
    /// Execution, output disposition and ownership remain unchanged.
    pub async fn record_tool_reconciliation(
        &self,
        fact: &ToolReconciliationFact,
    ) -> anyhow::Result<()> {
        let encoded = serde_json::to_string(&ToolRuntimeFact::Reconciliation(fact.clone()))?;
        ensure!(
            encoded.len() <= 16 * 1024,
            "reconciliation metadata exceeds limit"
        );
        let mut tx = self.pool.begin().await?;
        let current: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM tool_attempts a JOIN tool_requests r ON r.sequence = a.request_sequence WHERE r.thread_id = ? AND a.request_sequence = ? AND a.attempt_id = ? AND a.owner_id = ? AND a.operation_digest = ? AND a.revision = ?)")
            .bind(&fact.thread_id).bind(fact.request_sequence).bind(&fact.attempt.attempt_id)
            .bind(&fact.attempt.owner_id).bind(&fact.attempt.operation_digest).bind(fact.attempt.revision)
            .fetch_one(&mut *tx).await?;
        ensure!(current, "reconciliation receipt has changed");
        let previous: Option<String> = sqlx::query_scalar("SELECT fact_json FROM tool_fact_events WHERE request_sequence = ? AND json_extract(fact_json, '$.kind') = 'reconciliation' ORDER BY sequence DESC LIMIT 1")
            .bind(fact.request_sequence).fetch_optional(&mut *tx).await?;
        if previous.as_deref() != Some(&encoded) {
            sqlx::query("INSERT INTO tool_fact_events(request_sequence, thread_id, fact_json) VALUES (?, ?, ?)")
                .bind(fact.request_sequence).bind(&fact.thread_id).bind(encoded).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok(())
    }
}

#[cfg(test)]
#[path = "tool_reconciliation_tests.rs"]
mod tests;
