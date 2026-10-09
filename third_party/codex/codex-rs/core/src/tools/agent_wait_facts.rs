// SPDX-License-Identifier: Apache-2.0
use super::ExecutionFacts;
use super::nested_identity;
use super::storage_error;
use crate::function_tool::FunctionCallError;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use crate::tools::agent_wait_graph::CompletionWaitLease;
use crate::tools::agent_wait_graph::evidence;
use crate::tools::agent_wait_graph::unix_ms;
use crate::tools::context::ToolCallSource;
use codex_state::ToolAgentWaitFact;
use codex_state::ToolWaitEdge;
use codex_state::ToolWaitNode;
use codex_state::ToolWaitState;
use std::sync::Arc;
use tokio_util::task::task_tracker::TaskTrackerToken;

impl ExecutionFacts {
    pub(crate) async fn begin_agent_wait(
        session: &Session,
        _turn: &TurnContext,
        source: &ToolCallSource,
        targets: Vec<(codex_protocol::ThreadId, String)>,
        timeout_ms: u64,
    ) -> Result<Option<AgentWait>, FunctionCallError> {
        let ToolCallSource::CodeMode {
            cell_id,
            runtime_tool_call_id,
        } = source
        else {
            return Ok(None);
        };
        let facts = Self::for_session(session);
        let writer = facts.try_writer().ok_or_else(super::owner::closing_error)?;
        let (_, scope) = facts
            .parent(cell_id)
            .ok_or_else(|| storage_error("agent wait cell scope expired"))?;
        let owner = Self::current_owner(session);
        let store = facts
            .store
            .get()
            .ok_or_else(|| storage_error("agent wait store unavailable"))?;
        let cell = store
            .latest_tool_cell(&session.thread_id.to_string(), cell_id)
            .await
            .map_err(storage_error)?
            .ok_or_else(|| storage_error("agent wait cell unavailable"))?;
        if cell.owner_id != owner
            || cell.scope_id != scope
            || cell.lifecycle != codex_state::ToolCellLifecycle::Live
        {
            return Err(storage_error("agent wait cell owner unverified"));
        }
        let logical = nested_identity(cell_id, runtime_tool_call_id, &scope);
        let request = store
            .tool_request_fact(&session.thread_id.to_string(), &logical)
            .await
            .map_err(storage_error)?
            .ok_or_else(|| storage_error("agent wait request unavailable"))?;
        let edge = ToolWaitEdge {
            tree_id: session.services.agent_control.session_id().to_string(),
            request_sequence: request.request_sequence,
            logical_id: logical,
            source: ToolWaitNode::Cell {
                thread_id: session.thread_id.to_string(),
                owner_id: owner,
                cell_id: cell_id.clone(),
                scope_id: scope,
            },
            targets: targets
                .into_iter()
                .map(|(thread, owner)| ToolWaitNode::Thread {
                    thread_id: thread.to_string(),
                    owner_id: owner,
                })
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect(),
            deadline_unix_ms: unix_ms().saturating_add(timeout_ms),
        };
        let fact = ToolAgentWaitFact {
            schema_version: 1,
            thread_id: session.thread_id.to_string(),
            edge: edge.clone(),
            state: ToolWaitState::Waiting,
        };
        store
            .record_tool_agent_wait(&fact)
            .await
            .map_err(storage_error)?;
        let graph_lease = session
            .services
            .agent_control
            .completion_waits
            .insert(edge, evidence(&request));
        crate::tools::tool_diagnostics::ToolDiagnostics::refresh(session).await;
        Ok(Some(AgentWait {
            writer,
            facts,
            fact,
            _graph_lease: graph_lease,
            settled: false,
        }))
    }
}

pub(crate) struct AgentWait {
    writer: TaskTrackerToken,
    facts: Arc<ExecutionFacts>,
    fact: ToolAgentWaitFact,
    _graph_lease: CompletionWaitLease,
    settled: bool,
}
impl AgentWait {
    pub(crate) async fn settle(mut self) -> Result<(), FunctionCallError> {
        self.fact.state = ToolWaitState::Settled;
        self.facts
            .store
            .get()
            .ok_or_else(|| storage_error("agent wait store unavailable"))?
            .record_tool_agent_wait(&self.fact)
            .await
            .map_err(storage_error)?;
        self.settled = true;
        Ok(())
    }
}
impl Drop for AgentWait {
    fn drop(&mut self) {
        if self.settled {
            return;
        }
        let writer = self.writer.clone();
        let facts = Arc::clone(&self.facts);
        let mut fact = self.fact.clone();
        fact.state = ToolWaitState::Settled;
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let _writer = writer;
                if let Some(store) = facts.store.get()
                    && let Err(error) = store.record_tool_agent_wait(&fact).await
                {
                    facts.owner.fail(format!(
                        "agent wait cancellation was not persisted: {error}"
                    ));
                }
            });
        } else {
            self.facts
                .owner
                .fail("agent wait cancellation has no runtime".into());
        }
    }
}
