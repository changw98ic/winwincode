// SPDX-License-Identifier: Apache-2.0

use std::collections::HashMap;
use std::sync::Mutex;

use codex_code_mode::CellId;
use codex_code_mode::RuntimeResponse;
use codex_code_mode::WaitOutcome;
use tokio::sync::watch;

use super::CodeModeService;
use crate::function_tool::FunctionCallError;
use crate::tools::parallel::ToolContinuation;

#[derive(Clone)]
enum HostBoundary {
    Handoff(ToolContinuation),
    Fatal(String),
}

/// Latches typed host boundaries until the outer control call consumes them.
#[derive(Default)]
pub(crate) struct TerminalHandoffs {
    cells: Mutex<HashMap<CellId, watch::Sender<Option<HostBoundary>>>>,
}

impl TerminalHandoffs {
    pub(super) fn register(&self, cell_id: &CellId) {
        self.cells
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(cell_id.clone())
            .or_insert_with(|| watch::channel(None).0);
    }

    pub(super) fn discard_all(&self) {
        self.cells
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
    }

    pub(super) fn close_cell(&self, cell_id: &CellId) {
        let mut cells = self
            .cells
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if cells
            .get(cell_id)
            .is_some_and(|sender| sender.borrow().is_none())
        {
            cells.remove(cell_id);
        }
    }

    pub(crate) fn publish(
        &self,
        cell_id: CellId,
        continuation: ToolContinuation,
    ) -> Result<(), FunctionCallError> {
        let mut cells = self
            .cells
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let sender = cells.get_mut(&cell_id).ok_or_else(|| {
            FunctionCallError::RespondToModel(
                "code cell is no longer active for host handoff".to_string(),
            )
        })?;
        if sender.borrow().is_some() {
            return Err(FunctionCallError::RespondToModel(
                "code cell already handed control to the host".to_string(),
            ));
        }
        sender.send_replace(Some(HostBoundary::Handoff(continuation)));
        Ok(())
    }

    /// Retain the original typed fatal before the native runtime turns its
    /// tool error into a catchable JavaScript exception. A closed cell cannot
    /// regain authority, and the first fatal supersedes an earlier handoff.
    pub(super) fn publish_fatal(&self, cell_id: &CellId, message: String) {
        let mut cells = self
            .cells
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(sender) = cells.get_mut(cell_id) {
            let already_fatal = matches!(sender.borrow().as_ref(), Some(HostBoundary::Fatal(_)));
            if !already_fatal {
                sender.send_replace(Some(HostBoundary::Fatal(message)));
            }
        }
    }

    pub(crate) fn is_pending(&self, cell_id: &CellId) -> bool {
        self.cells
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(cell_id)
            .is_some_and(|sender| sender.borrow().is_some())
    }
}

impl CodeModeService {
    pub(super) async fn await_boundary(
        &self,
        cell_id: &CellId,
        response: impl std::future::Future<Output = Result<WaitOutcome, String>> + Send,
    ) -> Result<(WaitOutcome, Option<ToolContinuation>), FunctionCallError> {
        let receiver = {
            let cells = self
                .terminal_handoffs
                .cells
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            cells.get(cell_id).map(watch::Sender::subscribe)
        };
        let Some(mut receiver) = receiver else {
            return response
                .await
                .map(|outcome| (outcome, None))
                .map_err(FunctionCallError::RespondToModel);
        };
        let host_boundary = async {
            loop {
                if let Some(boundary) = receiver.borrow_and_update().clone() {
                    return boundary;
                }
                if receiver.changed().await.is_err() {
                    return std::future::pending().await;
                }
            }
        };
        let (boundary, observed_host_boundary) = tokio::select! {
            biased;
            host_boundary = host_boundary => {
                (self.terminate(cell_id.clone()).await, Some(host_boundary))
            }
            response = response => {
                let host_boundary = self.terminal_handoffs.cells.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
                    .get(cell_id).and_then(|sender| sender.borrow().clone());
                match host_boundary {
                    Some(host_boundary) => (self.terminate(cell_id.clone()).await, Some(host_boundary)),
                    None => (response, None),
                }
            }
        };
        // Inspect and close the boundary under one lock. A required-fact
        // failure outranks an observed handoff or a caught script exception.
        let host_boundary = {
            let mut cells = self
                .terminal_handoffs
                .cells
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let host_boundary = cells
                .get(cell_id)
                .and_then(|sender| sender.borrow().clone())
                .or(observed_host_boundary);
            if !matches!(host_boundary, Some(HostBoundary::Fatal(_)))
                && !matches!(
                    &boundary,
                    Ok(WaitOutcome::LiveCell(RuntimeResponse::Yielded { .. }))
                )
            {
                cells.remove(cell_id);
            }
            host_boundary
        };
        let continuation = match host_boundary {
            Some(HostBoundary::Fatal(message)) => {
                // Keep the cell fenced until normal turn interruption cleans
                // it up, even if runtime termination itself failed.
                return Err(FunctionCallError::Fatal(message));
            }
            Some(HostBoundary::Handoff(continuation)) => Some(continuation),
            None => None,
        };
        Ok((
            boundary.map_err(FunctionCallError::RespondToModel)?,
            continuation,
        ))
    }
}

#[cfg(test)]
#[path = "terminal_tests.rs"]
mod tests;
