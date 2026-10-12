// SPDX-License-Identifier: Apache-2.0
use super::StateRuntime;
use crate::ToolDiagnosticResponse;
use crate::ToolRuntimeFact;
use sqlx::Row;

impl StateRuntime {
    /// Associates an observed model response with diagnoses offered in this turn.
    pub async fn record_tool_diagnostic_response(
        &self,
        thread_id: &str,
        turn_id: &str,
        response_digest: &str,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(response_digest.len() == 64, "invalid model response digest");
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let rows = sqlx::query("SELECT diagnostic_id, evidence_version, boundary FROM (SELECT f.diagnostic_id, f.evidence_version, f.boundary_request_sequence AS boundary, ROW_NUMBER() OVER (PARTITION BY f.diagnostic_id ORDER BY f.evidence_version DESC, f.boundary_request_sequence DESC) AS ordinal FROM tool_diagnostic_feedback f JOIN tool_requests r ON r.sequence = f.boundary_request_sequence WHERE f.thread_id = ? AND r.request_json ->> '$.turn_id' = ? AND f.offered = 1) WHERE ordinal = 1")
            .bind(thread_id).bind(turn_id).fetch_all(&mut *tx).await?;
        for row in rows {
            let fact = ToolDiagnosticResponse {
                schema_version: 1,
                thread_id: thread_id.into(),
                turn_id: turn_id.into(),
                response_digest: response_digest.into(),
                diagnostic_id: row.try_get("diagnostic_id")?,
                evidence_version: row.try_get("evidence_version")?,
                boundary_request_sequence: row.try_get("boundary")?,
            };
            let inserted = sqlx::query("INSERT OR IGNORE INTO tool_diagnostic_responses(thread_id, diagnostic_id, evidence_version, turn_id, response_digest, boundary_request_sequence) VALUES (?, ?, ?, ?, ?, ?)")
                .bind(thread_id).bind(&fact.diagnostic_id).bind(fact.evidence_version).bind(turn_id).bind(response_digest).bind(fact.boundary_request_sequence).execute(&mut *tx).await?.rows_affected();
            if inserted == 1 {
                sqlx::query("INSERT INTO tool_fact_events(request_sequence, thread_id, fact_json) VALUES (?, ?, ?)")
                    .bind(fact.boundary_request_sequence).bind(thread_id).bind(serde_json::to_string(&ToolRuntimeFact::DiagnosticResponse(fact))?).execute(&mut *tx).await?;
            }
        }
        tx.commit().await?;
        Ok(())
    }
}
