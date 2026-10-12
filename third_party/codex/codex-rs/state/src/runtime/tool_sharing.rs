// SPDX-License-Identifier: Apache-2.0
use super::StateRuntime;
use crate::MAX_TOOL_RESULT_BYTES;
use crate::ToolCoalescingPermission;
use crate::ToolOutputDelivery;
use crate::ToolOutputDisposition;
use crate::ToolReusePermission;
use crate::ToolRuntimeFact;
use crate::ToolSharingFact;
use crate::ToolSharingKind;
use anyhow::ensure;
use sqlx::Row;
use sqlx::SqliteConnection;

impl StateRuntime {
    /// Links an unclaimed logical request to a trusted original attempt. The
    /// snapshot must exactly match the original frozen binding.
    pub async fn link_tool_sharing(&self, fact: &ToolSharingFact) -> anyhow::Result<()> {
        ensure!(
            fact.schema_version == 1
                && fact.request_sequence != fact.source_request_sequence
                && fact.disposition == ToolOutputDisposition::Pending
                && fact.delivery == ToolOutputDelivery::Pending
                && !fact.cancelled,
            "invalid initial tool sharing receipt"
        );
        ensure!(
            match fact.kind {
                ToolSharingKind::Reuse =>
                    fact.snapshot.reuse == ToolReusePermission::ImmutableValue,
                ToolSharingKind::Merged =>
                    fact.snapshot.coalescing == ToolCoalescingPermission::SharedRead,
            },
            "tool sharing is not permitted"
        );
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let row = sqlx::query("SELECT a.execution,a.disposition,b.validation FROM tool_attempts a JOIN tool_requests r ON r.sequence=a.request_sequence JOIN tool_input_bindings b ON b.request_sequence=a.request_sequence WHERE a.request_sequence=? AND a.attempt_id=? AND r.thread_id=? AND a.operation_digest=? AND b.snapshot_json=?")
            .bind(fact.source_request_sequence).bind(&fact.source_attempt_id).bind(&fact.thread_id)
            .bind(&fact.operation_digest).bind(serde_json::to_string(&fact.snapshot)?)
            .fetch_one(&mut *tx).await?;
        if fact.kind == ToolSharingKind::Reuse {
            ensure!(
                row.try_get::<&str, _>("execution")? == "completed"
                    && row.try_get::<&str, _>("disposition")? == "accepted"
                    && row.try_get::<&str, _>("validation")? == "verified",
                "source is not reusable"
            );
        }
        let unclaimed: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM tool_requests r WHERE r.sequence=? AND r.thread_id=? AND r.resolution='observed' AND NOT EXISTS(SELECT 1 FROM tool_attempts a WHERE a.request_sequence=r.sequence))")
            .bind(fact.request_sequence).bind(&fact.thread_id).fetch_one(&mut *tx).await?;
        ensure!(
            unclaimed,
            "sharing requires an independent unclaimed request"
        );
        let encoded = serde_json::to_string(fact)?;
        let inserted = sqlx::query("INSERT INTO tool_sharing(request_sequence,thread_id,source_request_sequence,fact_json) VALUES (?,?,?,?) ON CONFLICT(request_sequence) DO NOTHING")
            .bind(fact.request_sequence).bind(&fact.thread_id).bind(fact.source_request_sequence).bind(&encoded)
            .execute(&mut *tx).await?.rows_affected() == 1;
        let retained: String =
            sqlx::query_scalar("SELECT fact_json FROM tool_sharing WHERE request_sequence=?")
                .bind(fact.request_sequence)
                .fetch_one(&mut *tx)
                .await?;
        ensure!(retained == encoded, "sharing receipt conflict");
        if inserted {
            append(&mut tx, fact).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn tool_sharing_fact(
        &self,
        sequence: i64,
    ) -> anyhow::Result<Option<ToolSharingFact>> {
        sqlx::query_scalar::<_, String>(
            "SELECT fact_json FROM tool_sharing WHERE request_sequence=?",
        )
        .bind(sequence)
        .fetch_optional(self.pool.as_ref())
        .await?
        .map(|value| serde_json::from_str(&value).map_err(Into::into))
        .transpose()
    }

    /// Every logical caller runs its own output hooks before this decision.
    pub async fn decide_shared_tool_output(
        &self,
        sequence: i64,
        disposition: ToolOutputDisposition,
        body: &str,
    ) -> anyhow::Result<()> {
        ensure!(
            disposition != ToolOutputDisposition::Pending,
            "missing sharing output decision"
        );
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let mut fact = load(&mut tx, sequence).await?;
        ensure!(
            !fact.cancelled && fact.disposition == ToolOutputDisposition::Pending,
            "sharing output already decided"
        );
        let completed: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM tool_attempts WHERE request_sequence=? AND attempt_id=? AND execution='completed')")
            .bind(fact.source_request_sequence).bind(&fact.source_attempt_id).fetch_one(&mut *tx).await?;
        ensure!(
            disposition == ToolOutputDisposition::Rejected || completed,
            "shared execution is not complete"
        );
        fact.disposition = disposition;
        let retained = (body.len() <= MAX_TOOL_RESULT_BYTES).then_some(body);
        sqlx::query("UPDATE tool_sharing SET fact_json=?,accepted_result=?,rejection=? WHERE request_sequence=?")
            .bind(serde_json::to_string(&fact)?).bind(if disposition == ToolOutputDisposition::Accepted { retained } else { None })
            .bind(if disposition == ToolOutputDisposition::Rejected { retained } else { None }).bind(sequence).execute(&mut *tx).await?;
        append(&mut tx, &fact).await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn offer_shared_tool_output(&self, sequence: i64) -> anyhow::Result<()> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let mut fact = load(&mut tx, sequence).await?;
        ensure!(
            !fact.cancelled && fact.disposition == ToolOutputDisposition::Accepted,
            "shared output not accepted"
        );
        if fact.delivery == ToolOutputDelivery::Pending {
            fact.delivery = ToolOutputDelivery::Offered;
            update(&mut tx, &fact).await?;
        }
        tx.commit().await?;
        Ok(())
    }
}
async fn load(connection: &mut SqliteConnection, sequence: i64) -> anyhow::Result<ToolSharingFact> {
    let encoded: String =
        sqlx::query_scalar("SELECT fact_json FROM tool_sharing WHERE request_sequence=?")
            .bind(sequence)
            .fetch_one(connection)
            .await?;
    Ok(serde_json::from_str(&encoded)?)
}
async fn update(connection: &mut SqliteConnection, fact: &ToolSharingFact) -> anyhow::Result<()> {
    sqlx::query("UPDATE tool_sharing SET fact_json=? WHERE request_sequence=?")
        .bind(serde_json::to_string(fact)?)
        .bind(fact.request_sequence)
        .execute(&mut *connection)
        .await?;
    append(connection, fact).await
}
async fn append(connection: &mut SqliteConnection, fact: &ToolSharingFact) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO tool_fact_events(request_sequence,thread_id,fact_json) VALUES (?,?,?)",
    )
    .bind(fact.request_sequence)
    .bind(&fact.thread_id)
    .bind(serde_json::to_string(&ToolRuntimeFact::Sharing(
        fact.clone(),
    ))?)
    .execute(connection)
    .await?;
    Ok(())
}

impl StateRuntime {
    /// Private logical output. Authorize its original actual source under
    /// current read authority before reading, and revalidate before delivery.
    pub async fn read_shared_tool_output(&self, sequence: i64) -> anyhow::Result<Option<String>> {
        Ok(sqlx::query_scalar("SELECT accepted_result FROM tool_sharing WHERE request_sequence=? AND json_extract(fact_json,'$.disposition')='accepted' AND accepted_result IS NOT NULL")
            .bind(sequence).fetch_optional(self.pool.as_ref()).await?)
    }
}

impl StateRuntime {
    pub async fn cancel_tool_waiter(&self, sequence: i64) -> anyhow::Result<()> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let row = sqlx::query("SELECT r.thread_id,COALESCE(s.source_request_sequence,r.sequence) AS source FROM tool_requests r LEFT JOIN tool_sharing s ON s.request_sequence=r.sequence LEFT JOIN tool_attempts a ON a.request_sequence=r.sequence WHERE r.sequence=? AND (a.delivery='pending' OR json_extract(s.fact_json,'$.delivery')='pending')")
            .bind(sequence).fetch_optional(&mut *tx).await?;
        let Some(row) = row else {
            return Ok(());
        };
        let inserted = sqlx::query("INSERT INTO tool_waiter_cancellations(request_sequence) VALUES (?) ON CONFLICT(request_sequence) DO NOTHING")
            .bind(sequence).execute(&mut *tx).await?.rows_affected() == 1;
        if inserted {
            let fact = crate::ToolWaiterCancellationFact {
                schema_version: 1,
                thread_id: row.try_get("thread_id")?,
                request_sequence: sequence,
                source_request_sequence: row.try_get("source")?,
            };
            sqlx::query(
                "INSERT INTO tool_fact_events(request_sequence,thread_id,fact_json) VALUES (?,?,?)",
            )
            .bind(sequence)
            .bind(fact.thread_id.clone())
            .bind(serde_json::to_string(
                &ToolRuntimeFact::WaiterCancellation(fact),
            )?)
            .execute(&mut *tx)
            .await?;
        }
        let shared: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM tool_sharing WHERE request_sequence=?)",
        )
        .bind(sequence)
        .fetch_one(&mut *tx)
        .await?;
        if shared {
            let mut fact = load(&mut tx, sequence).await?;
            if !fact.cancelled && fact.delivery == ToolOutputDelivery::Pending {
                fact.cancelled = true;
                update(&mut tx, &fact).await?;
            }
        }
        tx.commit().await?;
        Ok(())
    }
}

impl StateRuntime {
    pub async fn tool_waiter_cancelled(&self, sequence: i64) -> anyhow::Result<bool> {
        Ok(sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM tool_waiter_cancellations WHERE request_sequence=?)",
        )
        .bind(sequence)
        .fetch_one(self.pool.as_ref())
        .await?)
    }
}
