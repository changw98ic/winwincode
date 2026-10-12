// SPDX-License-Identifier: Apache-2.0
use super::UnifiedExecProcessManager;
use crate::session::session::Session;
use codex_state::ToolRecoveryEvidence;

impl UnifiedExecProcessManager {
    /// A process receipt is scoped to its original Core session and call, not its command text.
    pub(crate) async fn reconcile_original_call(
        &self,
        session: &Session,
        call_id: &str,
    ) -> ToolRecoveryEvidence {
        let store = self.process_store.lock().await;
        let Some(entry) = store.processes.values().find(|entry| {
            entry.call_id == call_id
                && entry
                    .session
                    .upgrade()
                    .is_some_and(|owner| std::ptr::eq(owner.as_ref(), session))
        }) else {
            return ToolRecoveryEvidence::Unconfirmed;
        };
        let business_id = entry.process_id.to_string();
        if entry.process.has_exited() {
            ToolRecoveryEvidence::Exited {
                business_id,
                exit_code: entry.process.exit_code(),
            }
        } else {
            ToolRecoveryEvidence::Running { business_id }
        }
    }
}
