// SPDX-License-Identifier: Apache-2.0

use crate::session::session::Session;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::Ordering;
use tokio_util::task::TaskTracker;
use tokio_util::task::task_tracker::TaskTrackerToken;

/// Owns every admitted fact writer, including continuations spawned during Drop.
#[derive(Default)]
pub(super) struct ToolFactOwner {
    state: Mutex<OwnerState>,
    tracker: TaskTracker,
}

#[derive(Default)]
struct OwnerState {
    sealed: bool,
    failure: Option<String>,
    result: Option<Result<(), String>>,
}

impl ToolFactOwner {
    pub(super) fn admit(&self) -> Option<TaskTrackerToken> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.sealed {
            return None;
        }
        // Seal and token registration share one lock; drain cannot observe an admission gap.
        Some(self.tracker.token())
    }

    pub(super) fn seal(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.sealed = true;
        self.tracker.close();
    }

    pub(super) async fn drain(&self) {
        self.tracker.wait().await;
    }

    pub(super) fn fail(&self, error: String) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.failure.is_none() {
            state.failure = Some(error);
        }
    }

    pub(super) fn finish(&self, result: Result<(), String>) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.result = Some(result.and_then(|()| match &state.failure {
            Some(error) => Err(error.clone()),
            None => Ok(()),
        }));
    }

    pub(super) fn result(&self) -> Result<(), String> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .result
            .clone()
            .unwrap_or_else(|| Err("Core tool facts shutdown did not finish".into()))
    }
}

impl super::ExecutionFacts {
    pub(in crate::tools) fn try_writer(&self) -> Option<TaskTrackerToken> {
        self.owner.admit()
    }

    pub(crate) fn seal(session: &Session) {
        Self::for_session(session).owner.seal();
    }

    pub(crate) fn shutdown_result(session: &Session) -> codex_protocol::error::Result<()> {
        Self::for_session(session)
            .owner
            .result()
            .map_err(codex_protocol::error::CodexErr::Fatal)
    }

    pub(crate) async fn finish_shutdown(session: &Session, runtime_result: Result<(), String>) {
        let facts = Self::for_session(session);
        facts.owner.drain().await;
        let cells_result = if runtime_result.is_ok() {
            Self::close_owned_cells(session)
                .await
                .map_err(|error| error.to_string())
        } else {
            Ok(())
        };
        // sqlx may finish a queued COMMIT after its Rust future was cancelled.
        // Every fact append is transactional: this write lock fences those commits,
        // including direct-only sessions which have no cells to close.
        let fence_result = match facts.store.get().or(session.services.state_db.as_ref()) {
            Some(store) => store
                .flush_tool_runtime_events()
                .await
                .map_err(|error| error.to_string()),
            None => Ok(()),
        };
        facts
            .owner
            .finish(runtime_result.and(cells_result).and(fence_result));
    }
}
impl super::ToolFactRecord {
    pub(in crate::tools) fn persist_cancellation(&self) {
        // Derive the continuation token before releasing the record's token.
        // This is allowed after seal because it belongs to admitted work.
        let writer = self.writer.clone();
        let service = Arc::clone(&self.service);
        let store = Arc::clone(&self.store);
        let sequence = self.sequence;
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => {
                runtime.spawn(async move {
                    let _writer = writer;
                    if let Err(error) = store.cancel_tool_waiter(sequence).await {
                        service.owner.fail(format!(
                            "logical tool waiter cancellation was not persisted: {error}"
                        ));
                    }
                });
            }
            Err(error) => self.service.owner.fail(format!(
                "logical tool waiter cancellation has no runtime: {error}"
            )),
        }
    }
}

impl Drop for super::ToolFactRecord {
    fn drop(&mut self) {
        if (self.shared.load(Ordering::Acquire) || self.completed.load(Ordering::Acquire))
            && !self.settled.load(Ordering::Acquire)
        {
            self.persist_cancellation();
        }
        self.service
            .active_requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.sequence);
    }
}

pub(super) fn closing_error() -> crate::function_tool::FunctionCallError {
    crate::function_tool::FunctionCallError::RespondToModel(
        "tool_fact_owner_closed: Core tool facts owner is closing".into(),
    )
}
