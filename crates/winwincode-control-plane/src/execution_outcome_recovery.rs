// SPDX-License-Identifier: Apache-2.0

//! Server-owned cleanup consumes accepted outcomes without authenticating a dead Worker.

use crate::execution_outcome_commit::{AcceptedExecutionOutcome, retained_outcome};
use crate::{ControlPlane, DurableExecutionPortError};
use winwincode_domain::{RepositoryScope, RepositoryScopeKind};
use winwincode_storage::StorageError;

const PREFIX: &str = "execution-outcome:";
const PAGE_SIZE: usize = 64;

impl ControlPlane {
    /// Recovers one bounded page of accepted resource cleanup. Failed rows stay
    /// pending and are revisited after the cursor wraps; no execution is replayed.
    pub fn recover_accepted_execution_outcomes(
        &mut self,
    ) -> Result<usize, DurableExecutionPortError> {
        use DurableExecutionPortError::Storage;
        let storage = self.storage_ref().map_err(Storage)?;
        let Some(upper) = storage.last_state_stream_id(PREFIX).map_err(Storage)? else {
            self.outcome_recovery_cursor.clear();
            return Ok(0);
        };
        let rows = storage
            .scan_state_streams(PREFIX, &self.outcome_recovery_cursor, &upper, PAGE_SIZE)
            .map_err(Storage)?;
        let next = if rows.len() == PAGE_SIZE {
            rows.last()
                .map_or_else(String::new, |row| row.stream_id.clone())
        } else {
            String::new()
        };
        let mut completed = 0;
        for row in rows {
            if row.revision != 1 {
                continue;
            }
            let accepted: AcceptedExecutionOutcome =
                serde_json::from_slice(&row.payload).map_err(|_| {
                    Storage(StorageError::adapter(
                        "accepted outcome recovery record is invalid",
                    ))
                })?;
            // Revalidate the stored receipt and exact frame before using its
            // frozen cleanup authority. No incoming Worker message is involved.
            if row.stream_id
                != format!(
                    "execution-outcome:{}:{}",
                    accepted.message.lease.job_id.0, accepted.message.lease.attempt
                )
                || accepted.acknowledgement_ready
                || retained_outcome(self.storage_ref().map_err(Storage)?, &accepted.message)
                    .map_err(Storage)?
                    != Some(accepted.clone())
            {
                return Err(Storage(StorageError::adapter(
                    "accepted outcome recovery receipt differs",
                )));
            }
            if accepted
                .follow_up
                .as_ref()
                .is_some_and(|follow_up| follow_up.controller_required)
                && (self.delivery_authority.is_none() || self.delivery_dispatcher.is_none())
            {
                continue;
            }
            let scope = RepositoryScope {
                kind: RepositoryScopeKind::Repository,
                organization_id: accepted.scope.organization_id,
                workspace_id: accepted.scope.workspace_id,
                project_id: accepted.scope.project_id,
                repository_id: accepted.scope.repository_id,
            };
            match self.finish_accepted_execution_outcome(&scope, &accepted.message, false) {
                Ok(()) => completed += 1,
                Err(error) => {
                    if std::env::var_os("WWC_DEBUG_RUNTIME").is_some() {
                        eprintln!("accepted outcome cleanup remains pending: {error}");
                    }
                }
            }
        }
        self.outcome_recovery_cursor = next;
        Ok(completed)
    }
}
