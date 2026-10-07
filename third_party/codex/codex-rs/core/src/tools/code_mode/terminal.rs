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

/// Latches accepted host handoffs until the outer control call consumes them.
#[derive(Default)]
pub(crate) struct TerminalHandoffs {
    cells: Mutex<HashMap<CellId, watch::Sender<Option<ToolContinuation>>>>,
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
        sender.send_replace(Some(continuation));
        Ok(())
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
    ) -> Result<(WaitOutcome, Option<ToolContinuation>), String> {
        let receiver = {
            let cells = self
                .terminal_handoffs
                .cells
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            cells.get(cell_id).map(watch::Sender::subscribe)
        };
        let Some(mut receiver) = receiver else {
            return response.await.map(|outcome| (outcome, None));
        };
        let handoff = async {
            loop {
                if let Some(continuation) = receiver.borrow_and_update().clone() {
                    return continuation;
                }
                if receiver.changed().await.is_err() {
                    return std::future::pending().await;
                }
            }
        };
        let (boundary, continuation) = tokio::select! {
            biased;
            continuation = handoff => {
                (self.terminate(cell_id.clone()).await, Some(continuation))
            }
            response = response => {
                let continuation = self.terminal_handoffs.cells.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
                    .get(cell_id).and_then(|sender| sender.borrow().clone());
                match continuation {
                    Some(continuation) => (self.terminate(cell_id.clone()).await, Some(continuation)),
                    None => (response, None),
                }
            }
        };
        if !matches!(
            &boundary,
            Ok(WaitOutcome::LiveCell(RuntimeResponse::Yielded { .. }))
        ) {
            self.terminal_handoffs
                .cells
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(cell_id);
        }
        Ok((boundary?, continuation))
    }
}

#[cfg(test)]
#[path = "terminal_tests.rs"]
mod tests;
