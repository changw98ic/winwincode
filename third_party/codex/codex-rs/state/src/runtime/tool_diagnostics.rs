// SPDX-License-Identifier: Apache-2.0
use super::StateRuntime;
use crate::ToolDiagnostic;
use crate::ToolDiagnosticDelivery;
use crate::ToolDiagnosticFact;
use crate::ToolRuntimeFact;
use anyhow::ensure;
use sqlx::Row;
use sqlx::SqliteConnection;

impl StateRuntime {
    /// Stores only newer evidence under its stable diagnosis identity.
    pub async fn enqueue_tool_diagnostic(&self, diagnostic: &ToolDiagnostic) -> anyhow::Result<()> {
        let anchor = diagnostic
            .evidence
            .last()
            .ok_or_else(|| anyhow::anyhow!("diagnostic evidence is empty"))?
            .request_sequence;
        let encoded = serde_json::to_string(diagnostic)?;
        ensure!(
            encoded.len() <= 16 * 1024,
            "diagnostic exceeds metadata limit"
        );
        let mut tx = self.pool.begin().await?;
        let changed = sqlx::query("INSERT INTO tool_diagnostics(thread_id, diagnostic_id, evidence_version, anchor_request_sequence, diagnostic_json) VALUES (?, ?, ?, ?, ?) ON CONFLICT(thread_id, diagnostic_id) DO UPDATE SET evidence_version = excluded.evidence_version, anchor_request_sequence = excluded.anchor_request_sequence, diagnostic_json = excluded.diagnostic_json WHERE excluded.evidence_version > tool_diagnostics.evidence_version")
            .bind(&diagnostic.thread_id).bind(&diagnostic.diagnostic_id).bind(diagnostic.evidence_version).bind(anchor).bind(encoded).execute(&mut *tx).await?.rows_affected();
        if changed == 1 {
            append(
                &mut tx,
                anchor,
                diagnostic,
                ToolDiagnosticDelivery::Queued,
                None,
            )
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// A staged item is offered only with its boundary's accepted output receipt.
    /// An interrupted boundary leaves the diagnostic available for a later boundary.
    pub async fn stage_tool_diagnostic_feedback(
        &self,
        thread_id: &str,
        boundary_logical_id: &str,
    ) -> anyhow::Result<Vec<ToolDiagnostic>> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let boundary: i64 = sqlx::query_scalar("SELECT r.sequence FROM tool_requests r JOIN tool_attempts a ON a.request_sequence = r.sequence WHERE r.thread_id = ? AND r.logical_id = ? AND a.execution = 'running'")
            .bind(thread_id).bind(boundary_logical_id).fetch_one(&mut *tx).await?;
        let rows = sqlx::query("SELECT diagnostic_id, evidence_version, diagnostic_json FROM tool_diagnostics WHERE thread_id = ? AND evidence_version > offered_version ORDER BY evidence_version, diagnostic_id LIMIT 2")
            .bind(thread_id).fetch_all(&mut *tx).await?;
        let mut diagnostics = Vec::new();
        for row in rows {
            let encoded: String = row.try_get("diagnostic_json")?;
            let diagnostic: ToolDiagnostic = serde_json::from_str(&encoded)?;
            sqlx::query("INSERT OR IGNORE INTO tool_diagnostic_feedback(boundary_request_sequence, thread_id, diagnostic_id, evidence_version, diagnostic_json) VALUES (?, ?, ?, ?, ?)")
                .bind(boundary).bind(thread_id).bind(&diagnostic.diagnostic_id).bind(diagnostic.evidence_version).bind(encoded).execute(&mut *tx).await?;
            diagnostics.push(diagnostic);
        }
        tx.commit().await?;
        Ok(diagnostics)
    }
}

impl StateRuntime {
    /// Records optional feedback after its boundary's required output offer commits.
    /// Errors and cancellation discard the connection after possible SQLite auto-rollback.
    pub(super) async fn offer_staged_diagnostics(&self, boundary: i64) -> anyhow::Result<()> {
        let mut connection = self.pool.acquire().await?;
        connection.close_on_drop();
        let mut tx = sqlx::Connection::begin_with(&mut *connection, "BEGIN IMMEDIATE").await?;
        let rows = sqlx::query("UPDATE tool_diagnostic_feedback SET offered = 1 WHERE boundary_request_sequence = ? AND offered = 0 RETURNING diagnostic_json")
            .bind(boundary).fetch_all(&mut *tx).await?;
        for row in rows {
            let diagnostic: ToolDiagnostic = serde_json::from_str(row.try_get("diagnostic_json")?)?;
            sqlx::query("UPDATE tool_diagnostics SET offered_version = MAX(offered_version, ?) WHERE thread_id = ? AND diagnostic_id = ?")
                .bind(diagnostic.evidence_version).bind(&diagnostic.thread_id).bind(&diagnostic.diagnostic_id).execute(&mut *tx).await?;
            append(
                &mut tx,
                boundary,
                &diagnostic,
                ToolDiagnosticDelivery::Offered,
                Some(boundary),
            )
            .await?;
        }
        tx.commit().await?;
        // SQLx 0.9 exposes this explicitly; only a successfully committed
        // connection may return to the pool with close_on_drop set.
        connection.return_to_pool().await;
        Ok(())
    }
}

async fn append(
    connection: &mut SqliteConnection,
    anchor: i64,
    diagnostic: &ToolDiagnostic,
    delivery: ToolDiagnosticDelivery,
    boundary_request_sequence: Option<i64>,
) -> anyhow::Result<()> {
    let fact = ToolRuntimeFact::Diagnostic(ToolDiagnosticFact {
        schema_version: 1,
        diagnostic: diagnostic.clone(),
        delivery,
        boundary_request_sequence,
    });
    sqlx::query(
        "INSERT INTO tool_fact_events(request_sequence, thread_id, fact_json) VALUES (?, ?, ?)",
    )
    .bind(anchor)
    .bind(&diagnostic.thread_id)
    .bind(serde_json::to_string(&fact)?)
    .execute(connection)
    .await?;
    Ok(())
}

#[cfg(test)]
#[path = "tool_diagnostics_tests.rs"]
mod tests;
