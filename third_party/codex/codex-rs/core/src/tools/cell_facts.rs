// SPDX-License-Identifier: Apache-2.0
use super::ExecutionFacts;
use super::digest;
use super::storage_error;
use crate::function_tool::FunctionCallError;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use std::sync::Arc;

impl ExecutionFacts {
    pub(crate) async fn start_cell(
        session: &Session,
        turn: &TurnContext,
        cell_id: &str,
        call_id: &str,
    ) -> Result<(), FunctionCallError> {
        let facts = Self::for_session(session);
        let _writer = facts.try_writer().ok_or_else(super::owner::closing_error)?;
        let store = facts
            .store
            .get()
            .ok_or_else(|| storage_error("cell has no durable dispatch owner"))?;
        let owner = facts
            .owner_id
            .get()
            .ok_or_else(|| storage_error("cell owner is unavailable"))?;
        let scope = digest(serde_json::json!([turn.sub_id, call_id]));
        store
            .open_tool_cell(
                &session.thread_id.to_string(),
                &format!("direct:{scope}"),
                cell_id,
                &scope,
                owner,
            )
            .await
            .map_err(storage_error)?;
        facts.register_cell(cell_id.into(), call_id.into(), &turn.sub_id);
        Ok(())
    }

    pub(crate) async fn close_cell(
        session: &Session,
        cell_id: &str,
    ) -> Result<(), FunctionCallError> {
        let facts = Self::for_session(session);
        let _writer = facts.try_writer().ok_or_else(super::owner::closing_error)?;
        Self::close_cell_inner(session, &facts, cell_id).await
    }

    async fn close_cell_inner(
        session: &Session,
        facts: &ExecutionFacts,
        cell_id: &str,
    ) -> Result<(), FunctionCallError> {
        let Some((_, scope)) = facts.parent(cell_id) else {
            return Ok(());
        };
        let store = facts
            .store
            .get()
            .ok_or_else(|| storage_error("cell store is unavailable"))?;
        let owner = facts
            .owner_id
            .get()
            .ok_or_else(|| storage_error("cell owner is unavailable"))?;
        store
            .close_tool_cell(&session.thread_id.to_string(), cell_id, &scope, owner)
            .await
            .map_err(storage_error)
    }

    pub(crate) async fn close_owned_cells(session: &Session) -> Result<(), FunctionCallError> {
        let facts = Self::for_session(session);
        let ids: Vec<String> = facts
            .cells
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .keys()
            .cloned()
            .collect();
        for id in ids {
            Self::close_cell_inner(session, &facts, &id).await?;
        }
        Ok(())
    }

    pub(crate) async fn begin_cell_wait(
        session: &Session,
        turn: &TurnContext,
        cell_id: &str,
        call_id: &str,
        timeout_ms: u64,
    ) -> Result<Option<ToolCellWait>, FunctionCallError> {
        let facts = Self::for_session(session);
        let writer = facts.try_writer().ok_or_else(super::owner::closing_error)?;
        let Some((_, scope)) = facts.parent(cell_id) else {
            if let Some(store) = facts.store.get()
                && let Some(cell) = store
                    .latest_tool_cell(&session.thread_id.to_string(), cell_id)
                    .await
                    .map_err(storage_error)?
            {
                let state = if cell.lifecycle == codex_state::ToolCellLifecycle::Closed {
                    "cell_closed"
                } else {
                    "cell_owner_unverified"
                };
                crate::tools::tool_diagnostics::ToolDiagnostics::unavailable_wait(
                    session, turn, call_id, cell_id, state,
                )
                .await;
                return Err(FunctionCallError::RespondToModel(serde_json::json!({
                    "type": "code_cell_recovery", "schema_version": 1, "state": state, "cell": cell,
                }).to_string()));
            }
            crate::tools::tool_diagnostics::ToolDiagnostics::unavailable_wait(
                session,
                turn,
                call_id,
                cell_id,
                "cell_not_found",
            )
            .await;
            return Ok(None);
        };
        let store = facts
            .store
            .get()
            .ok_or_else(|| storage_error("cell store is unavailable"))?;
        let owner = facts
            .owner_id
            .get()
            .ok_or_else(|| storage_error("cell owner is unavailable"))?;
        let logical = format!(
            "direct:{}",
            digest(serde_json::json!([turn.sub_id, call_id]))
        );
        let wait = store
            .begin_tool_cell_wait(
                &session.thread_id.to_string(),
                &logical,
                cell_id,
                &scope,
                owner,
            )
            .await
            .map_err(storage_error)?;
        let Some(wait) = wait else {
            crate::tools::tool_diagnostics::ToolDiagnostics::unavailable_wait(
                session,
                turn,
                call_id,
                cell_id,
                "cell_closed",
            )
            .await;
            return Ok(None);
        };
        let request = store
            .tool_request_fact(&session.thread_id.to_string(), &logical)
            .await
            .map_err(storage_error)?
            .ok_or_else(|| storage_error("wait request missing"))?;
        let graph_lease = session.services.agent_control.completion_waits.insert(
            codex_state::ToolWaitEdge {
                tree_id: session.services.agent_control.session_id().to_string(),
                request_sequence: request.request_sequence,
                logical_id: logical,
                source: codex_state::ToolWaitNode::Thread {
                    thread_id: session.thread_id.to_string(),
                    owner_id: owner.clone(),
                },
                targets: vec![codex_state::ToolWaitNode::Cell {
                    thread_id: session.thread_id.to_string(),
                    owner_id: owner.clone(),
                    cell_id: cell_id.into(),
                    scope_id: scope,
                }],
                deadline_unix_ms: crate::tools::agent_wait_graph::unix_ms()
                    .saturating_add(timeout_ms),
            },
            crate::tools::agent_wait_graph::evidence(&request),
        );
        crate::tools::tool_diagnostics::ToolDiagnostics::refresh(session).await;
        Ok(Some(ToolCellWait {
            _graph_lease: graph_lease,
            _writer: writer,
            facts: Arc::clone(&facts),
            thread_id: session.thread_id.to_string(),
            sequence: wait.waiter_request_sequence,
        }))
    }
}

/// Dropping an interrupted waiter preserves its unresolved durable edge.
pub(crate) struct ToolCellWait {
    _graph_lease: crate::tools::agent_wait_graph::CompletionWaitLease,
    _writer: tokio_util::task::task_tracker::TaskTrackerToken,
    facts: Arc<ExecutionFacts>,
    thread_id: String,
    sequence: i64,
}
impl ToolCellWait {
    pub(crate) async fn settle(self) -> Result<(), FunctionCallError> {
        let store = self
            .facts
            .store
            .get()
            .ok_or_else(|| storage_error("wait store is unavailable"))?;
        let owner = self
            .facts
            .owner_id
            .get()
            .ok_or_else(|| storage_error("wait owner is unavailable"))?;
        store
            .settle_tool_cell_wait(&self.thread_id, self.sequence, owner)
            .await
            .map_err(storage_error)
    }
}
