// SPDX-License-Identifier: Apache-2.0

use super::StateRuntime;
use super::tool_execution::FACT_SELECT;
use super::tool_execution::decode_fact;
use super::tool_execution::load_fact;
use crate::ToolExecutionFact;
use crate::ToolReconciliationFact;
use crate::ToolRecoveryEvidence;
use crate::ToolRuntimeFact;
use anyhow::ensure;
use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use sqlx::QueryBuilder;
use sqlx::Sqlite;

/// Private answer bytes are deliberately absent from Debug and runtime projections.
#[derive(Clone, Deserialize, Serialize)]
pub struct ControlInputRecovery {
    pub result_ref: String,
    pub parent: ToolExecutionFact,
    pub input: ToolExecutionFact,
    pub response_digest: String,
    pub response_json: String,
}

#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
pub struct ControlInputRecoveryHeader {
    pub result_ref: String,
    pub parent: ToolExecutionFact,
    pub input: ToolExecutionFact,
    pub response_digest: String,
    pub total_bytes: usize,
    pub page_count: usize,
}

/// One private page. It has no Debug implementation and no projection event.
pub struct ControlInputResultPage {
    pub cursor: String,
    pub page_json: String,
    pub page_digest: String,
}

fn private_string_digest(value: &str) -> anyhow::Result<String> {
    Ok(format!(
        "{:x}",
        sha2::Sha256::digest(serde_json::to_vec(value)?)
    ))
}

impl StateRuntime {
    /// Select only the original pending control call in its exact original cell.
    /// Multiple pending controls are ambiguous and are never guessed or replayed.
    pub async fn pending_control_input_for_parent(
        &self,
        parent: &ToolExecutionFact,
        parent_call_id: &str,
    ) -> anyhow::Result<Option<ToolExecutionFact>> {
        let Some(attempt) = &parent.attempt else {
            return Ok(None);
        };
        let mut query = QueryBuilder::<Sqlite>::new(FACT_SELECT);
        query.push(" JOIN tool_runtime_cells c ON c.thread_id=r.thread_id AND c.cell_id=json_extract(r.request_json,'$.cell_id') AND c.scope_id=json_extract(r.request_json,'$.scope_id') AND c.owner_id=a.owner_id WHERE c.parent_request_sequence=")
            .push_bind(parent.request_sequence)
            .push(" AND r.thread_id=").push_bind(&parent.request.thread_id)
            .push(" AND json_extract(r.request_json,'$.turn_id')=").push_bind(&parent.request.turn_id)
            .push(" AND json_extract(r.request_json,'$.parent_call_id')=").push_bind(parent_call_id)
            .push(" AND json_extract(r.request_json,'$.tool_name')='request_user_input' AND json_extract(r.request_json,'$.source')='code_mode' AND a.owner_id=").push_bind(&attempt.owner_id)
            .push(" AND a.execution IN ('running','uncertain') AND a.disposition='pending' LIMIT 2");
        let rows = query.build().fetch_all(self.pool.as_ref()).await?;
        ensure!(rows.len() <= 1, "control_input_recovery_ambiguous");
        rows.first().map(decode_fact).transpose()
    }

    /// Caller authorizes both receipt reads before reading any private answer.
    pub async fn control_input_recovery(
        &self,
        thread: &str,
        result_ref: &str,
    ) -> anyhow::Result<Option<ControlInputRecovery>> {
        let encoded: Option<String> = sqlx::query_scalar("SELECT record_json FROM tool_control_input_recoveries WHERE thread_id=? AND result_ref=? AND sealed=1 AND invalidated=0")
            .bind(thread).bind(result_ref).fetch_optional(self.pool.as_ref()).await?;
        encoded
            .map(|value| serde_json::from_str(&value).map_err(Into::into))
            .transpose()
    }

    /// Payload-free metadata enables exact authorization before reading answer bytes.
    pub async fn control_input_recovery_receipts(
        &self,
        thread: &str,
        result_ref: &str,
    ) -> anyhow::Result<Option<(ToolExecutionFact, ToolExecutionFact)>> {
        let Some(header) = self
            .control_input_recovery_header(thread, result_ref)
            .await?
        else {
            return Ok(None);
        };
        Ok(Some((
            self.tool_execution_fact(header.parent.request_sequence)
                .await?,
            self.tool_execution_fact(header.input.request_sequence)
                .await?,
        )))
    }

    /// Reads only the payload-free immutable header, never the full answer.
    pub async fn control_input_recovery_header(
        &self,
        thread: &str,
        result_ref: &str,
    ) -> anyhow::Result<Option<ControlInputRecoveryHeader>> {
        let row: Option<(String,i64,i64)> = sqlx::query_as("SELECT metadata_json,sealed,invalidated FROM tool_control_input_recoveries WHERE thread_id=? AND result_ref=?")
            .bind(thread).bind(result_ref).fetch_optional(self.pool.as_ref()).await?;
        let Some((encoded, sealed, invalidated)) = row else {
            return Ok(None);
        };
        ensure!(
            sealed == 1 && invalidated == 0,
            "control_input_result_seal_invalid"
        );
        Ok(Some(serde_json::from_str(&encoded)?))
    }

    /// One indexed private page row is the entire answer read at this boundary.
    pub async fn control_input_result_page(
        &self,
        thread: &str,
        result_ref: &str,
        cursor: &str,
    ) -> anyhow::Result<Option<ControlInputResultPage>> {
        let row: Option<(String,String)> = sqlx::query_as("SELECT p.page_json,p.page_digest FROM tool_control_input_result_pages p JOIN tool_control_input_recoveries r ON r.input_request_sequence=p.input_request_sequence WHERE r.thread_id=? AND r.result_ref=? AND r.sealed=1 AND r.invalidated=0 AND p.cursor=?")
            .bind(thread).bind(result_ref).bind(cursor).fetch_optional(self.pool.as_ref()).await?;
        Ok(row.map(|(page_json, page_digest)| ControlInputResultPage {
            cursor: cursor.into(),
            page_json,
            page_digest,
        }))
    }

    /// Full private answer and payload-free evidence commit in one transaction.
    /// No attempt, original owner, output disposition or cell lifecycle is changed.
    pub async fn record_control_input_recovery(
        &self,
        record: &ControlInputRecovery,
        pages: &[ControlInputResultPage],
        fact: &ToolReconciliationFact,
    ) -> anyhow::Result<()> {
        ensure!(
            record.response_json.len() <= crate::MAX_TOOL_RESULT_BYTES,
            "control input response exceeds private retention limit"
        );
        ensure!(
            record.response_digest == private_string_digest(&record.response_json)?,
            "control input response digest mismatch"
        );
        ensure!(
            record.parent.request_sequence == fact.request_sequence
                && record.parent.request.thread_id == fact.thread_id
                && record.parent.attempt.as_ref() == Some(&fact.attempt)
                && record.input.request.thread_id == fact.thread_id
                && record.input.request.turn_id == record.parent.request.turn_id,
            "control recovery parent mismatch"
        );
        ensure!(
            matches!(&fact.evidence,ToolRecoveryEvidence::ControlInputResolved {
            input_request_sequence,input_operation_digest,response_digest,result_ref,continuation_lost
        } if *input_request_sequence==record.input.request_sequence
            && record.input.attempt.as_ref().is_some_and(|attempt|&attempt.operation_digest==input_operation_digest)
            && response_digest==&record.response_digest && result_ref==&record.result_ref && *continuation_lost),
            "control recovery evidence binding mismatch"
        );
        let encoded = serde_json::to_string(record)?;
        ensure!(!pages.is_empty(), "control input pages missing");
        let mut reconstructed = String::new();
        for (index, page) in pages.iter().enumerate() {
            ensure!(
                page.page_json.len() <= 8192,
                "control input page exceeds limit"
            );
            ensure!(
                page.page_digest == private_string_digest(&page.page_json)?,
                "control input page digest mismatch"
            );
            let value: serde_json::Value = serde_json::from_str(&page.page_json)?;
            ensure!(
                value["resultRef"] == record.result_ref
                    && value["responseDigest"] == record.response_digest
                    && value["cursor"] == page.cursor
                    && value["totalBytes"] == record.response_json.len(),
                "control input page binding mismatch"
            );
            ensure!(
                if index == 0 {
                    page.cursor == "0"
                } else {
                    !page.cursor.is_empty()
                },
                "control input page cursor invalid"
            );
            ensure!(
                value["nextCursor"].as_str()
                    == pages.get(index + 1).map(|next| next.cursor.as_str()),
                "control input page sequence invalid"
            );
            reconstructed.push_str(
                value["content"]
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("control input page content invalid"))?,
            );
        }
        ensure!(
            reconstructed == record.response_json,
            "control input pages do not preserve full response"
        );
        let header = ControlInputRecoveryHeader {
            result_ref: record.result_ref.clone(),
            parent: record.parent.clone(),
            input: record.input.clone(),
            response_digest: record.response_digest.clone(),
            total_bytes: record.response_json.len(),
            page_count: pages.len(),
        };
        let metadata = serde_json::to_string(&header)?;
        ensure!(
            metadata.len() <= 16 * 1024,
            "control input recovery header exceeds limit"
        );
        let evidence = serde_json::to_string(&ToolRuntimeFact::Reconciliation(fact.clone()))?;
        ensure!(
            evidence.len() <= 16 * 1024,
            "control recovery metadata exceeds limit"
        );
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        ensure!(
            load_fact(&mut tx, record.parent.request_sequence).await? == record.parent
                && load_fact(&mut tx, record.input.request_sequence).await? == record.input,
            "control recovery receipt changed"
        );
        let previous: Option<(String,i64,i64)> = sqlx::query_as(
            "SELECT record_json,sealed,invalidated FROM tool_control_input_recoveries WHERE input_request_sequence=?",
        )
        .bind(record.input.request_sequence)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some((previous, sealed, invalidated)) = previous {
            ensure!(
                previous == encoded && sealed == 1 && invalidated == 0,
                "control recovery response conflict"
            );
        } else {
            sqlx::query("INSERT INTO tool_control_input_recoveries(input_request_sequence,parent_request_sequence,thread_id,result_ref,record_json,metadata_json,page_count) VALUES(?,?,?,?,?,?,?)")
                .bind(record.input.request_sequence).bind(record.parent.request_sequence).bind(&record.parent.request.thread_id).bind(&record.result_ref).bind(encoded).bind(metadata).bind(i64::try_from(pages.len())?).execute(&mut *tx).await?;
            for page in pages {
                sqlx::query("INSERT INTO tool_control_input_result_pages(input_request_sequence,cursor,page_json,page_digest) VALUES(?,?,?,?)")
                    .bind(record.input.request_sequence).bind(&page.cursor).bind(&page.page_json).bind(&page.page_digest).execute(&mut *tx).await?;
            }
            let published=sqlx::query("UPDATE tool_control_input_recoveries SET sealed=1 WHERE input_request_sequence=? AND sealed=0 AND invalidated=0 AND (SELECT COUNT(*) FROM tool_control_input_result_pages WHERE input_request_sequence=?)=page_count")
                .bind(record.input.request_sequence).bind(record.input.request_sequence).execute(&mut *tx).await?;
            ensure!(
                published.rows_affected() == 1,
                "control input publication failed"
            );
            sqlx::query(
                "INSERT INTO tool_fact_events(request_sequence,thread_id,fact_json) VALUES(?,?,?)",
            )
            .bind(fact.request_sequence)
            .bind(&fact.thread_id)
            .bind(evidence)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }
}

#[cfg(test)]
#[path = "tool_control_input_recovery_tests.rs"]
mod tests;
