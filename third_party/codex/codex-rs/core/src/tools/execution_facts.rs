// SPDX-License-Identifier: Apache-2.0

#[path = "cell_facts.rs"]
mod cell_facts;

use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use codex_state::StateRuntime;
use codex_state::ToolAttemptClaim;
use codex_state::ToolExecutionFact;
use codex_state::ToolExecutionStatus;
use codex_state::ToolOutputDisposition;
use codex_state::ToolRequestIdentity;
use codex_state::ToolRequestObservation;
use codex_state::ToolRequestResolution;
use sha2::Digest;
use tokio::sync::OnceCell;

use super::context::ToolCallSource;
use super::context::ToolInvocation;
use super::context::ToolPayload;
use super::registry::AnyToolResult;
use super::registry::CoreToolRuntime;
use crate::function_tool::FunctionCallError;
use crate::session::session::Session;

/// Core-owned process incarnation and durable receipts, shared by all tool families.
#[derive(Default)]
pub(crate) struct ExecutionFacts {
    pub(super) store: OnceCell<Arc<StateRuntime>>,
    pub(super) owner_id: OnceCell<String>,
    cells: Mutex<HashMap<String, (String, String)>>,
    pub(super) active_requests: Mutex<HashSet<i64>>,
}

impl ExecutionFacts {
    pub(super) fn for_session(session: &Session) -> Arc<Self> {
        session
            .services
            .thread_extension_data
            .get_or_init(Self::default)
    }

    pub(crate) async fn read_events(
        session: &Session,
        after: i64,
        limit: u32,
    ) -> anyhow::Result<Vec<codex_state::ToolRuntimeEvent>> {
        let facts = Self::for_session(session);
        let store = facts.store.get().or(session.services.state_db.as_ref());
        match store {
            Some(store) => {
                store
                    .list_tool_runtime_events(&session.thread_id.to_string(), after, limit)
                    .await
            }
            None => Ok(Vec::new()),
        }
    }

    pub(super) fn register_cell(&self, cell_id: String, parent_call_id: String, turn_id: &str) {
        self.cells
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                cell_id,
                (
                    parent_call_id.clone(),
                    digest(serde_json::json!([turn_id, parent_call_id])),
                ),
            );
    }

    pub(super) fn parent(&self, cell_id: &str) -> Option<(String, String)> {
        self.cells
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(cell_id)
            .cloned()
    }
}

/// Held only for the new logical request. A replay never executes hooks or the handler.
pub(super) struct ToolFactRecord {
    service: Arc<ExecutionFacts>,
    pub(super) store: Arc<StateRuntime>,
    pub(super) sequence: i64,
    pub(super) owner_id: String,
    pub(super) attempt_id: String,
    shared: AtomicBool,
    completed: AtomicBool,
    settled: AtomicBool,
}

pub(super) enum ToolFactAdmission {
    New(ToolFactRecord),
    Replay(ToolFactReplay),
}

pub(super) struct ToolFactReplay {
    pub(super) service: Arc<ExecutionFacts>,
    pub(super) store: Arc<StateRuntime>,
    pub(super) fact: Box<ToolExecutionFact>,
    pub(super) owner_id: String,
}

impl ToolFactReplay {
    pub(super) fn response(&self) -> FunctionCallError {
        let active = self
            .service
            .active_requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(&self.fact.request_sequence);
        recovery_response(&self.fact, active, &self.owner_id)
    }
}

impl ToolFactRecord {
    pub(super) async fn begin(
        invocation: &ToolInvocation,
    ) -> Result<ToolFactAdmission, FunctionCallError> {
        let service = ExecutionFacts::for_session(&invocation.session);
        let owner_id = service
            .owner_id
            .get_or_init(|| async { uuid::Uuid::new_v4().to_string() })
            .await
            .clone();
        let store = service
            .store
            .get_or_try_init(|| async {
                if let Some(store) = &invocation.session.services.state_db {
                    Ok(Arc::clone(store))
                } else {
                    StateRuntime::init(
                        invocation.turn.config.sqlite_config().clone(),
                        invocation.turn.config.model_provider_id.clone(),
                    )
                    .await
                }
            })
            .await
            .map_err(storage_error)?
            .clone();
        let (logical_id, cell_id, parent_call_id, scope_id, source) = match &invocation.source {
            ToolCallSource::Direct => (
                direct_identity(invocation),
                None,
                None,
                invocation.turn.sub_id.clone(),
                "model",
            ),
            ToolCallSource::DirectPlaintextMessage => (
                direct_identity(invocation),
                None,
                None,
                invocation.turn.sub_id.clone(),
                "plaintext",
            ),
            ToolCallSource::CodeMode {
                cell_id,
                runtime_tool_call_id,
            } => {
                let (parent, scope) = service.parent(cell_id).ok_or_else(|| {
                    FunctionCallError::RespondToModel(
                        "cell_scope_expired: no current Core owner for this code cell".into(),
                    )
                })?;
                (
                    nested_identity(cell_id, runtime_tool_call_id, &scope),
                    Some(cell_id.clone()),
                    Some(parent),
                    scope,
                    "code_mode",
                )
            }
        };
        let request = ToolRequestIdentity {
            thread_id: invocation.session.thread_id.to_string(),
            logical_id,
            turn_id: invocation.turn.sub_id.clone(),
            scope_id,
            cell_id,
            parent_call_id,
            tool_name: invocation.tool_name.to_string(),
            source: source.into(),
            binding: digest(payload_value(&invocation.payload)),
        };
        match store.observe_tool_request(&request).await.map_err(storage_error)? {
            ToolRequestObservation::New(fact) => Ok(ToolFactAdmission::New(Self {
                service: service.clone(),
                store,
                sequence: fact.request_sequence,
                owner_id,
                attempt_id: uuid::Uuid::new_v4().to_string(),
                shared: AtomicBool::new(false),
                completed: AtomicBool::new(false),
                settled: AtomicBool::new(false),
            })),
            ToolRequestObservation::Replay(fact) => {
                Ok(ToolFactAdmission::Replay(ToolFactReplay { service, store, fact: Box::new(fact), owner_id }))
            }
            ToolRequestObservation::IdentityConflict => {
                Err(FunctionCallError::RespondToModel(
                    "tool_request_identity_conflict: logical request identity is bound to different input or provenance".into()
                ))
            }
        }
    }

    pub(super) async fn deny(&self) -> Result<(), FunctionCallError> {
        self.settled.store(true, Ordering::Release);
        self.store
            .deny_tool_request(self.sequence)
            .await
            .map_err(storage_error)
    }

    pub(super) async fn claim(
        &self,
        invocation: &ToolInvocation,
        runtime: &dyn CoreToolRuntime,
    ) -> Result<(), FunctionCallError> {
        let operation = operation_digest(invocation, runtime);
        match self
            .store
            .claim_tool_attempt(
                self.sequence,
                &self.attempt_id,
                &self.owner_id,
                &operation,
                &payload_value(&invocation.payload).to_string(),
            )
            .await
            .map_err(storage_error)?
        {
            ToolAttemptClaim::Claimed(_) => {
                self.service
                    .active_requests
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert(self.sequence);
                Ok(())
            }
            ToolAttemptClaim::Existing(fact) => {
                Err(recovery_response(&fact, false, &self.owner_id))
            }
        }
    }

    pub(super) async fn link_shared(
        &self,
        fact: codex_state::ToolSharingFact,
    ) -> Result<(), FunctionCallError> {
        self.store
            .link_tool_sharing(&fact)
            .await
            .map_err(storage_error)?;
        self.shared.store(true, Ordering::Release);
        Ok(())
    }

    pub(super) async fn completed(&self, encoded: Option<String>) -> Result<(), FunctionCallError> {
        if self.shared.load(Ordering::Acquire) || self.completed.load(Ordering::Acquire) {
            return Ok(());
        }
        let execution = if encoded.is_some() {
            ToolExecutionStatus::Completed
        } else {
            ToolExecutionStatus::Uncertain
        };
        self.store
            .complete_tool_attempt(
                self.sequence,
                &self.attempt_id,
                &self.owner_id,
                execution,
                encoded.as_deref(),
            )
            .await
            .map_err(storage_error)?;
        self.completed.store(true, Ordering::Release);
        Ok(())
    }

    pub(super) async fn rejected(&self, message: &str) -> Result<(), FunctionCallError> {
        self.decide_output(ToolOutputDisposition::Rejected, message)
            .await
    }

    pub(super) async fn accepted(&self, encoded: String) -> Result<(), FunctionCallError> {
        self.decide_output(ToolOutputDisposition::Accepted, &encoded)
            .await
    }

    async fn decide_output(
        &self,
        disposition: ToolOutputDisposition,
        body: &str,
    ) -> Result<(), FunctionCallError> {
        if self.shared.load(Ordering::Acquire) {
            self.store
                .decide_shared_tool_output(self.sequence, disposition, body)
                .await
                .map_err(storage_error)?;
        } else {
            self.store
                .decide_tool_output(
                    self.sequence,
                    &self.attempt_id,
                    &self.owner_id,
                    disposition,
                    Some(body),
                )
                .await
                .map_err(storage_error)?;
        }
        self.settled.store(true, Ordering::Release);
        Ok(())
    }

    pub(super) async fn offered(&self) -> Result<(), FunctionCallError> {
        if self.shared.load(Ordering::Acquire) {
            return self
                .store
                .offer_shared_tool_output(self.sequence)
                .await
                .map_err(storage_error);
        }
        self.store
            .offer_tool_output(self.sequence, &self.attempt_id, &self.owner_id)
            .await
            .map_err(storage_error)
    }
}

impl Drop for ToolFactRecord {
    fn drop(&mut self) {
        if (self.shared.load(Ordering::Acquire) || self.completed.load(Ordering::Acquire))
            && !self.settled.load(Ordering::Acquire)
            && let Ok(runtime) = tokio::runtime::Handle::try_current()
        {
            let store = Arc::clone(&self.store);
            let sequence = self.sequence;
            runtime.spawn(async move {
                if let Err(error) = store.cancel_tool_waiter(sequence).await {
                    tracing::warn!(%error, "logical tool waiter cancellation was not persisted");
                }
            });
        }
        self.service
            .active_requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.sequence);
    }
}

pub(super) fn operation_digest(
    invocation: &ToolInvocation,
    runtime: &dyn CoreToolRuntime,
) -> String {
    digest(
        serde_json::json!({"tool": invocation.tool_name.to_string(), "definition": runtime.spec(), "input": semantic_input(&invocation.payload)}),
    )
}

pub(super) fn snapshot(result: &AnyToolResult) -> Result<String, FunctionCallError> {
    // Preserve both projections: PostToolUse feedback can replace the direct output
    // while Code Mode still receives the original structured value.
    let response = result
        .result
        .to_response_item(&result.call_id, &result.payload);
    let response_success = match &response {
        codex_protocol::models::ResponseInputItem::FunctionCallOutput { output, .. }
        | codex_protocol::models::ResponseInputItem::CustomToolCallOutput { output, .. } => {
            output.success
        }
        _ => None,
    };
    serde_json::to_string(&serde_json::json!({
        "schema_version": 1,
        "response": response,
        "response_success": response_success,
        "code_mode_result": result.result.code_mode_result(&result.payload),
        "success": result.result.success_for_logging(),
        "effective_input": payload_value(&result.payload),
        "continuation": result.continuation,
        "post_hook_input": result.post_tool_use_payload.as_ref().map(|p| &p.tool_input),
        "post_hook_response": result.post_tool_use_payload.as_ref().map(|p| &p.tool_response),
    }))
    .map_err(storage_error)
}

fn direct_identity(invocation: &ToolInvocation) -> String {
    format!(
        "direct:{}",
        digest(serde_json::json!([
            invocation.turn.sub_id,
            invocation.call_id
        ]))
    )
}

pub(super) fn nested_identity(cell_id: &str, runtime_call_id: &str, scope_id: &str) -> String {
    format!(
        "code-mode:{}",
        digest(serde_json::json!([scope_id, cell_id, runtime_call_id]))
    )
}

pub(super) fn nested_call_id(
    session: &Session,
    cell_id: &str,
    runtime_call_id: &str,
) -> Result<String, FunctionCallError> {
    let (_, scope) = ExecutionFacts::for_session(session)
        .parent(cell_id)
        .ok_or_else(|| {
            FunctionCallError::RespondToModel(
                "cell_scope_expired: no current Core owner for this code cell".into(),
            )
        })?;
    Ok(nested_identity(cell_id, runtime_call_id, &scope))
}

pub(super) fn payload_value(payload: &ToolPayload) -> serde_json::Value {
    match payload {
        // Keep exact bytes for transport identity, including whitespace.
        ToolPayload::Function { arguments } => {
            serde_json::json!({"kind": "function", "input": arguments})
        }
        ToolPayload::Custom { input } => serde_json::json!({"kind": "custom", "input": input}),
        ToolPayload::ToolSearch { arguments } => {
            serde_json::json!({"kind": "tool_search", "input": arguments})
        }
    }
}

pub(super) fn semantic_input(payload: &ToolPayload) -> serde_json::Value {
    let mut value = payload_value(payload);
    if let ToolPayload::Function { arguments } = payload {
        value["input"] =
            serde_json::from_str(arguments).unwrap_or_else(|_| serde_json::json!(arguments));
    }
    value
}

pub(super) fn digest(mut value: serde_json::Value) -> String {
    value.sort_all_objects();
    format!("{:x}", sha2::Sha256::digest(value.to_string().as_bytes()))
}

pub(super) fn storage_error(error: impl std::fmt::Display) -> FunctionCallError {
    tracing::warn!(%error, "tool execution fact storage failed");
    FunctionCallError::RespondToModel(
        "tool_fact_store_unavailable: execution and recovery require durable Core facts".into(),
    )
}

pub(super) fn recovery_response(
    fact: &ToolExecutionFact,
    active: bool,
    current_owner: &str,
) -> FunctionCallError {
    let state = match &fact.attempt {
        None if fact.resolution == ToolRequestResolution::Denied => "request_denied",
        None => "not_dispatched",
        Some(attempt) => match (attempt.execution, attempt.disposition) {
            (ToolExecutionStatus::Running, _) if active => "in_flight",
            (ToolExecutionStatus::Running, _) if attempt.owner_id != current_owner => {
                "execution_owner_unverified"
            }
            (ToolExecutionStatus::Running | ToolExecutionStatus::Uncertain, _) => {
                "reconciliation_required"
            }
            (ToolExecutionStatus::Completed, ToolOutputDisposition::Pending) => {
                "output_decision_pending"
            }
            (ToolExecutionStatus::Completed, ToolOutputDisposition::Rejected) => "output_rejected",
            (ToolExecutionStatus::Completed, ToolOutputDisposition::Accepted)
                if !attempt.accepted_result_retained =>
            {
                "stored_result_unavailable"
            }
            (ToolExecutionStatus::Completed, ToolOutputDisposition::Accepted) => {
                "stored_result_requires_validation"
            }
        },
    };
    // Historical facts do not assert that cells or downstream process handles are live.
    FunctionCallError::RespondToModel(
        serde_json::json!({
            "type": "tool_recovery", "schema_version": 1, "state": state,
            "request_sequence": fact.request_sequence,
            "attempt": fact.attempt,
        })
        .to_string(),
    )
}

#[cfg(test)]
#[path = "execution_facts_tests.rs"]
mod tests;
