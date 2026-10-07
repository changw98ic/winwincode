// SPDX-License-Identifier: Apache-2.0
use super::execution_facts::ExecutionFacts;
use crate::context::ContextualUserFragment;
use crate::context::ToolReceipt;
use crate::context::ToolReceipts;
use crate::session::session::Session;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_state::ToolOutputDelivery;
use codex_state::ToolOutputDisposition;

pub(super) async fn feedback(
    session: &Session,
    cell_id: &str,
) -> Option<FunctionCallOutputContentItem> {
    let facts = ExecutionFacts::for_session(session);
    let store = facts.store.get()?;
    let (_, scope) = facts.parent(cell_id)?;
    let requests = match store
        .list_tool_cell_receipts(&session.thread_id.to_string(), cell_id, &scope)
        .await
    {
        Ok(requests) => requests,
        Err(error) => {
            tracing::warn!(%error, "Core tool receipt references unavailable");
            return None;
        }
    };
    let mut receipts = Vec::new();
    for fact in requests {
        if fact.request.source != "code_mode" || fact.request.logical_id.len() > 80 {
            continue;
        }
        let shared = match store.tool_sharing_fact(fact.request_sequence).await {
            Ok(shared) => shared,
            Err(error) => {
                tracing::warn!(%error, "shared receipt reference unavailable");
                continue;
            }
        };
        let attempt = fact.attempt.as_ref();
        receipts.push(ToolReceipt {
            source_id: fact.request.logical_id,
            tool: fact.request.tool_name.chars().take(64).collect(),
            request_sequence: fact.request_sequence,
            execution: attempt.map(|attempt| attempt.execution),
            disposition: shared
                .as_ref()
                .map(|shared| shared.disposition)
                .or_else(|| attempt.map(|attempt| attempt.disposition))
                .unwrap_or(ToolOutputDisposition::Pending),
            delivery: shared
                .as_ref()
                .map(|shared| shared.delivery)
                .or_else(|| attempt.map(|attempt| attempt.delivery))
                .unwrap_or(ToolOutputDelivery::Pending),
            source_request_sequence: shared.map(|shared| shared.source_request_sequence),
        });
    }
    if receipts.is_empty() {
        return None;
    }
    Some(FunctionCallOutputContentItem::InputText {
        text: ToolReceipts(receipts).render(),
    })
}
