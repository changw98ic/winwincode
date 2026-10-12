// SPDX-License-Identifier: Apache-2.0

use super::{StateRuntime, append_event, load_fact};
use sqlx::Row;
use std::fs::{File, OpenOptions, TryLockError};

impl StateRuntime {
    /// A process-held lock for one thread. Its lifetime must cover every fact
    /// writer. A released lock is proof that no cooperating owner can still write.
    pub fn try_tool_owner_lock(&self, thread: &str) -> anyhow::Result<Option<File>> {
        let root = self.sqlite().home().join("tool-owner-locks");
        std::fs::create_dir_all(&root)?;
        let name: String = thread
            .as_bytes()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(root.join(name))?;
        match file.try_lock() {
            Ok(()) => Ok(Some(file)),
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Error(error)) => Err(error.into()),
        }
    }

    /// Close interrupted logical calls after exclusive thread ownership is
    /// acquired. Physical effects remain uncertain; completed bodies are retained.
    pub async fn fail_pending_tool_calls(
        &self,
        thread: &str,
        owner: Option<&str>,
    ) -> anyhow::Result<u64> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let rows = sqlx::query("SELECT r.sequence,a.execution FROM tool_requests r LEFT JOIN tool_attempts a ON a.request_sequence=r.sequence
            WHERE r.thread_id=? AND (r.call_status='pending' OR (r.original_input IS NOT NULL AND (a.execution='running' OR a.disposition='pending'))) AND (? IS NULL OR r.request_owner=?)")
            .bind(thread).bind(owner).bind(owner).fetch_all(&mut *tx).await?;
        for row in &rows {
            let sequence: i64 = row.try_get("sequence")?;
            let execution: Option<String> = row.try_get("execution")?;
            let result = serde_json::json!({"status":"failed","code":"tool_execution_interrupted",
                "effects":if execution.is_none() {"not_started"} else {"unknown"}})
            .to_string();
            sqlx::query("UPDATE tool_attempts SET execution='uncertain',execution_result=?,disposition='rejected',rejection=?,revision=revision+1
                WHERE request_sequence=? AND execution='running'")
                .bind(&result).bind(&result).bind(sequence).execute(&mut *tx).await?;
            sqlx::query(
                "UPDATE tool_attempts SET disposition='rejected',rejection=?,revision=revision+1
                WHERE request_sequence=? AND execution!='running' AND disposition='pending'",
            )
            .bind(&result)
            .bind(sequence)
            .execute(&mut *tx)
            .await?;
            let shared: Option<String> =
                sqlx::query_scalar("SELECT fact_json FROM tool_sharing WHERE request_sequence=?")
                    .bind(sequence)
                    .fetch_optional(&mut *tx)
                    .await?;
            if let Some(shared) = shared {
                let mut fact: crate::ToolSharingFact = serde_json::from_str(&shared)?;
                fact.disposition = crate::ToolOutputDisposition::Rejected;
                sqlx::query("UPDATE tool_sharing SET rejection=? WHERE request_sequence=?")
                    .bind(&result)
                    .bind(sequence)
                    .execute(&mut *tx)
                    .await?;
                super::super::tool_sharing::update(&mut tx, &fact).await?;
            }
            sqlx::query("UPDATE tool_requests SET call_status='failed',call_result=? WHERE sequence=? AND call_status='pending'")
                .bind(&result).bind(sequence).execute(&mut *tx).await?;
            let fact = load_fact(&mut tx, sequence).await?;
            append_event(&mut tx, &fact).await?;
        }
        tx.commit().await?;
        Ok(rows.len() as u64)
    }

    /// Called at startup or by verified process recovery. Live threads retain
    /// their OS locks and are skipped even when they share this database.
    pub async fn recover_abandoned_tool_calls(&self) -> anyhow::Result<u64> {
        let threads: Vec<String> = sqlx::query_scalar("SELECT DISTINCT r.thread_id FROM tool_requests r LEFT JOIN tool_attempts a ON a.request_sequence=r.sequence WHERE r.call_status='pending' OR (r.original_input IS NOT NULL AND (a.execution='running' OR a.disposition='pending'))")
            .fetch_all(self.pool.as_ref()).await?;
        let mut closed = 0;
        for thread in threads {
            if let Some(_owner) = self.try_tool_owner_lock(&thread)? {
                closed += self.fail_pending_tool_calls(&thread, None).await?;
            }
        }
        Ok(closed)
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{observe, setup};
    use super::*;
    use crate::{ToolExecutionStatus, ToolOutputDisposition};

    #[tokio::test]
    async fn abandoned_pending_calls_fail_once_live_calls_and_late_results_are_fenced() {
        let (store, request) = setup().await;
        let owner = store
            .try_tool_owner_lock(&request.thread_id)
            .unwrap()
            .unwrap();
        let input = "{\"kind\":\"function\",\"input\":\"  exact request bytes \\n\"}";
        let crate::ToolRequestObservation::New(fact) = store
            .observe_tool_request(&request, "old-owner", input)
            .await
            .unwrap()
        else {
            panic!("new")
        };
        store
            .claim_tool_attempt(
                fact.request_sequence,
                "old-attempt",
                "old-owner",
                "digest",
                "hook input",
            )
            .await
            .unwrap();
        let other = StateRuntime::init(store.sqlite().clone(), "test".into())
            .await
            .unwrap();
        assert_eq!(other.recover_abandoned_tool_calls().await.unwrap(), 0);
        drop(owner);
        assert_eq!(other.recover_abandoned_tool_calls().await.unwrap(), 1);
        assert_eq!(other.recover_abandoned_tool_calls().await.unwrap(), 0);
        let row = sqlx::query(
            "SELECT original_input,call_status,call_result FROM tool_requests WHERE sequence=?",
        )
        .bind(fact.request_sequence)
        .fetch_one(other.pool.as_ref())
        .await
        .unwrap();
        assert_eq!(row.get::<String, _>("original_input"), input);
        assert_eq!(row.get::<String, _>("call_status"), "failed");
        assert!(row.get::<String, _>("call_result").contains("unknown"));
        let failed = other
            .tool_execution_fact(fact.request_sequence)
            .await
            .unwrap();
        assert_eq!(
            failed.attempt.unwrap().execution,
            ToolExecutionStatus::Uncertain
        );
        assert!(
            store
                .complete_tool_attempt(
                    fact.request_sequence,
                    "old-attempt",
                    "old-owner",
                    ToolExecutionStatus::Completed,
                    Some("late success")
                )
                .await
                .is_err()
        );
        assert!(
            store
                .decide_tool_output(
                    fact.request_sequence,
                    "old-attempt",
                    "old-owner",
                    ToolOutputDisposition::Accepted,
                    Some("late success")
                )
                .await
                .is_err()
        );
        assert!(
            !serde_json::to_string(
                &store
                    .tool_execution_fact(fact.request_sequence)
                    .await
                    .unwrap()
            )
            .unwrap()
            .contains("exact request bytes")
        );
    }

    #[tokio::test]
    async fn call_pair_and_receipt_roll_back_on_output_or_recovery_failure() {
        let (store, request) = setup().await;
        let fact = observe(&store, &request).await;
        store
            .claim_tool_attempt(
                fact.request_sequence,
                "attempt",
                "owner",
                "digest",
                "actual input",
            )
            .await
            .unwrap();
        store
            .complete_tool_attempt(
                fact.request_sequence,
                "attempt",
                "owner",
                ToolExecutionStatus::Completed,
                Some("actual result"),
            )
            .await
            .unwrap();
        sqlx::query("CREATE TRIGGER fail_call_pair BEFORE UPDATE OF call_result ON tool_requests BEGIN SELECT RAISE(ABORT,'injected'); END;")
            .execute(store.pool.as_ref()).await.unwrap();
        assert!(
            store
                .decide_tool_output(
                    fact.request_sequence,
                    "attempt",
                    "owner",
                    ToolOutputDisposition::Accepted,
                    Some("accepted result")
                )
                .await
                .is_err()
        );
        let physical = store
            .tool_execution_fact(fact.request_sequence)
            .await
            .unwrap()
            .attempt
            .unwrap();
        assert_eq!(physical.disposition, ToolOutputDisposition::Pending);
        assert!(store.recover_abandoned_tool_calls().await.is_err());
        assert_eq!(
            store
                .tool_execution_fact(fact.request_sequence)
                .await
                .unwrap()
                .attempt
                .unwrap(),
            physical
        );
        sqlx::query("DROP TRIGGER fail_call_pair")
            .execute(store.pool.as_ref())
            .await
            .unwrap();
        assert_eq!(store.recover_abandoned_tool_calls().await.unwrap(), 1);
        let retained = store
            .read_tool_results(fact.request_sequence)
            .await
            .unwrap();
        assert_eq!(retained.execution.as_deref(), Some("actual result"));
        assert_eq!(retained.effective_input, "actual input");
        assert!(retained.rejection.unwrap().contains("interrupted"));
    }

    #[tokio::test]
    async fn recovery_closes_cancelled_physical_attempts_and_fences_unclaimed_calls() {
        let (store, request) = setup().await;
        let fact = observe(&store, &request).await;
        store
            .claim_tool_attempt(
                fact.request_sequence,
                "cancelled-attempt",
                "owner",
                "digest",
                "actual input",
            )
            .await
            .unwrap();
        store
            .cancel_tool_waiter(fact.request_sequence)
            .await
            .unwrap();
        let cancelled: String =
            sqlx::query_scalar("SELECT call_result FROM tool_requests WHERE sequence=?")
                .bind(fact.request_sequence)
                .fetch_one(store.pool.as_ref())
                .await
                .unwrap();
        assert_eq!(store.recover_abandoned_tool_calls().await.unwrap(), 1);
        let row = sqlx::query("SELECT call_status,call_result FROM tool_requests WHERE sequence=?")
            .bind(fact.request_sequence)
            .fetch_one(store.pool.as_ref())
            .await
            .unwrap();
        assert_eq!(row.get::<String, _>("call_status"), "cancelled");
        assert_eq!(row.get::<String, _>("call_result"), cancelled);
        assert_eq!(
            store
                .tool_execution_fact(fact.request_sequence)
                .await
                .unwrap()
                .attempt
                .unwrap()
                .execution,
            ToolExecutionStatus::Uncertain
        );
        assert!(
            store
                .complete_tool_attempt(
                    fact.request_sequence,
                    "cancelled-attempt",
                    "owner",
                    ToolExecutionStatus::Completed,
                    Some("late physical success")
                )
                .await
                .is_err()
        );

        let mut unclaimed = request.clone();
        unclaimed.logical_id = "unclaimed".into();
        let unclaimed = observe(&store, &unclaimed).await;
        assert_eq!(store.recover_abandoned_tool_calls().await.unwrap(), 1);
        assert!(
            matches!(store.claim_tool_attempt(unclaimed.request_sequence, "late-attempt", "owner", "digest", "late input").await.unwrap(), crate::ToolAttemptClaim::Existing(fact) if fact.attempt.is_none())
        );
        let failure: String =
            sqlx::query_scalar("SELECT call_result FROM tool_requests WHERE sequence=?")
                .bind(unclaimed.request_sequence)
                .fetch_one(store.pool.as_ref())
                .await
                .unwrap();
        assert!(failure.contains("not_started"));
        assert_eq!(store.recover_abandoned_tool_calls().await.unwrap(), 0);

        let mut retry = request;
        retry.logical_id = "new-attempt-call".into();
        let retry = observe(&store, &retry).await;
        assert!(matches!(
            store
                .claim_tool_attempt(
                    retry.request_sequence,
                    "new-attempt",
                    "new-owner",
                    "digest",
                    "actual input"
                )
                .await
                .unwrap(),
            crate::ToolAttemptClaim::Claimed(_)
        ));
    }
}
