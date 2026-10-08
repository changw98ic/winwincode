// SPDX-License-Identifier: Apache-2.0
use super::StateRuntime;
use crate::ToolCellFact;
use crate::ToolRuntimeEvent;
use crate::ToolRuntimeFact;
use crate::ToolWaitFact;
use crate::ToolWaitState;
use anyhow::ensure;
use sqlx::Row;
use sqlx::SqliteConnection;

impl StateRuntime {
    /// Waits for fact transactions whose COMMIT outlived a cancelled Rust future.
    /// Fact writers must be sealed and drained before acquiring this barrier.
    pub async fn flush_tool_runtime_events(&self) -> anyhow::Result<()> {
        let tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        tx.commit().await?;
        Ok(())
    }

    /// Binds a host-created cell to its already claimed outer exec request.
    pub async fn open_tool_cell(
        &self,
        thread_id: &str,
        parent_logical_id: &str,
        cell_id: &str,
        scope_id: &str,
        owner_id: &str,
    ) -> anyhow::Result<ToolCellFact> {
        ensure!(
            cell_id.len() <= 1024 && scope_id.len() <= 1024,
            "cell identity exceeds limit"
        );
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let parent: i64 = sqlx::query_scalar("SELECT r.sequence FROM tool_requests r JOIN tool_attempts a ON a.request_sequence = r.sequence WHERE r.thread_id = ? AND r.logical_id = ? AND a.owner_id = ? AND a.execution = 'running'")
            .bind(thread_id).bind(parent_logical_id).bind(owner_id).fetch_one(&mut *tx).await?;
        let row = sqlx::query("INSERT INTO tool_runtime_cells(thread_id, parent_request_sequence, cell_id, scope_id, owner_id, lifecycle) VALUES (?, ?, ?, ?, ?, 'live') RETURNING *")
            .bind(thread_id).bind(parent).bind(cell_id).bind(scope_id).bind(owner_id).fetch_one(&mut *tx).await?;
        let fact = cell_fact(&row)?;
        append(
            &mut tx,
            parent,
            thread_id,
            &ToolRuntimeFact::Cell(fact.clone()),
        )
        .await?;
        tx.commit().await?;
        Ok(fact)
    }

    /// A wait edge is recorded only for a live cell owned by the same Core incarnation.
    pub async fn begin_tool_cell_wait(
        &self,
        thread_id: &str,
        waiter_logical_id: &str,
        cell_id: &str,
        scope_id: &str,
        owner_id: &str,
    ) -> anyhow::Result<Option<ToolWaitFact>> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let waiter: i64 = sqlx::query_scalar("SELECT r.sequence FROM tool_requests r JOIN tool_attempts a ON a.request_sequence = r.sequence WHERE r.thread_id = ? AND r.logical_id = ? AND a.owner_id = ? AND a.execution = 'running'")
            .bind(thread_id).bind(waiter_logical_id).bind(owner_id).fetch_one(&mut *tx).await?;
        let target: Option<i64> = sqlx::query_scalar("SELECT sequence FROM tool_runtime_cells WHERE thread_id = ? AND cell_id = ? AND scope_id = ? AND owner_id = ? AND lifecycle = 'live'")
            .bind(thread_id).bind(cell_id).bind(scope_id).bind(owner_id).fetch_optional(&mut *tx).await?;
        let Some(target) = target else {
            return Ok(None);
        };
        sqlx::query("INSERT INTO tool_runtime_waits(waiter_request_sequence, target_cell_sequence, owner_id, state) VALUES (?, ?, ?, 'waiting')")
            .bind(waiter).bind(target).bind(owner_id).execute(&mut *tx).await?;
        let fact = ToolWaitFact {
            schema_version: 1,
            thread_id: thread_id.into(),
            waiter_request_sequence: waiter,
            target_cell_sequence: target,
            owner_id: owner_id.into(),
            state: ToolWaitState::Waiting,
            revision: 1,
        };
        append(
            &mut tx,
            waiter,
            thread_id,
            &ToolRuntimeFact::Wait(fact.clone()),
        )
        .await?;
        tx.commit().await?;
        Ok(Some(fact))
    }

    /// Settlement records a delivered wait boundary, including a normal yield.
    pub async fn settle_tool_cell_wait(
        &self,
        thread_id: &str,
        waiter: i64,
        owner_id: &str,
    ) -> anyhow::Result<()> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let row = sqlx::query("UPDATE tool_runtime_waits SET state = 'settled', revision = revision + 1 WHERE waiter_request_sequence = ? AND owner_id = ? AND state = 'waiting' AND EXISTS(SELECT 1 FROM tool_requests WHERE sequence = ? AND thread_id = ?) RETURNING target_cell_sequence, revision")
            .bind(waiter).bind(owner_id).bind(waiter).bind(thread_id).fetch_one(&mut *tx).await?;
        let fact = ToolWaitFact {
            schema_version: 1,
            thread_id: thread_id.into(),
            waiter_request_sequence: waiter,
            target_cell_sequence: row.try_get("target_cell_sequence")?,
            owner_id: owner_id.into(),
            state: ToolWaitState::Settled,
            revision: row.try_get("revision")?,
        };
        append(&mut tx, waiter, thread_id, &ToolRuntimeFact::Wait(fact)).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Only an observed terminal runtime boundary closes a cell. Another owner
    /// cannot infer termination merely by reopening the database.
    pub async fn close_tool_cell(
        &self,
        thread_id: &str,
        cell_id: &str,
        scope_id: &str,
        owner_id: &str,
    ) -> anyhow::Result<()> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let row = sqlx::query("UPDATE tool_runtime_cells SET lifecycle = 'closed', revision = revision + 1 WHERE thread_id = ? AND cell_id = ? AND scope_id = ? AND owner_id = ? AND lifecycle = 'live' RETURNING *")
            .bind(thread_id).bind(cell_id).bind(scope_id).bind(owner_id).fetch_optional(&mut *tx).await?;
        if let Some(row) = row {
            let fact = cell_fact(&row)?;
            append(
                &mut tx,
                fact.parent_request_sequence,
                thread_id,
                &ToolRuntimeFact::Cell(fact),
            )
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Historical cell metadata does not imply that its owner is still live.
    pub async fn latest_tool_cell(
        &self,
        thread_id: &str,
        cell_id: &str,
    ) -> anyhow::Result<Option<ToolCellFact>> {
        let row = sqlx::query("SELECT * FROM tool_runtime_cells WHERE thread_id = ? AND cell_id = ? ORDER BY sequence DESC LIMIT 1")
            .bind(thread_id).bind(cell_id).fetch_optional(self.pool.as_ref()).await?;
        row.as_ref().map(cell_fact).transpose()
    }

    /// A single cursor rebuilds all payload-free runtime projections.
    pub async fn list_tool_runtime_events(
        &self,
        thread_id: &str,
        after: i64,
        limit: u32,
    ) -> anyhow::Result<Vec<ToolRuntimeEvent>> {
        let rows = sqlx::query("SELECT sequence, fact_json FROM tool_fact_events WHERE thread_id = ? AND sequence > ? ORDER BY sequence LIMIT ?")
            .bind(thread_id).bind(after).bind(limit.clamp(1, 200)).fetch_all(self.pool.as_ref()).await?;
        rows.into_iter()
            .map(|row| {
                Ok(ToolRuntimeEvent {
                    sequence: row.try_get("sequence")?,
                    fact: serde_json::from_str(row.try_get("fact_json")?)?,
                })
            })
            .collect()
    }
}

fn cell_fact(row: &sqlx::sqlite::SqliteRow) -> anyhow::Result<ToolCellFact> {
    Ok(ToolCellFact {
        schema_version: 1,
        sequence: row.try_get("sequence")?,
        thread_id: row.try_get("thread_id")?,
        parent_request_sequence: row.try_get("parent_request_sequence")?,
        cell_id: row.try_get("cell_id")?,
        scope_id: row.try_get("scope_id")?,
        owner_id: row.try_get("owner_id")?,
        lifecycle: serde_json::from_value(serde_json::json!(row.try_get::<&str, _>("lifecycle")?))?,
        revision: row.try_get("revision")?,
    })
}

async fn append(
    connection: &mut SqliteConnection,
    request: i64,
    thread: &str,
    fact: &ToolRuntimeFact,
) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO tool_fact_events(request_sequence, thread_id, fact_json) VALUES (?, ?, ?)",
    )
    .bind(request)
    .bind(thread)
    .bind(serde_json::to_string(fact)?)
    .execute(connection)
    .await?;
    Ok(())
}

#[cfg(test)]
#[path = "tool_runtime_tests.rs"]
mod tests;
