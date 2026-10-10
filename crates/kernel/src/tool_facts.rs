// SPDX-License-Identifier: Apache-2.0
use super::{Kernel, KernelFailure, KernelResult};

/// One original Core metadata event. Its cursor survives Kernel subscriptions
/// and Worker restarts; the JSON contains no tool input or result bodies.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KernelToolRuntimeEvent {
    pub source_sequence: i64,
    pub fact_json: String,
}

impl Kernel {
    /// Reads one bounded page from the Core-owned execution fact stream.
    ///
    /// # Errors
    ///
    /// Rejects a Core storage/encoding failure. Closed threads remain readable
    /// from this Kernel's private Core database so final receipts can drain.
    pub async fn tool_runtime_events(
        &self,
        session_id: &str,
        after: i64,
        limit: u32,
    ) -> KernelResult<Vec<KernelToolRuntimeEvent>> {
        let runtime = self.runtime().await?;
        let session = runtime.sessions.read().await.get(session_id).cloned();
        let facts = if let Some(session) = session {
            session.thread.tool_runtime_events(after, limit).await
        } else {
            let store = runtime.state_db.as_ref().ok_or_else(|| {
                KernelFailure::new(
                    "TOOL_FACT_READ_FAILED",
                    "Core execution fact storage is unavailable",
                )
            })?;
            store
                .list_tool_runtime_events(session_id, after, limit)
                .await
        }
        .map_err(|_| {
            KernelFailure::new(
                "TOOL_FACT_READ_FAILED",
                "Core execution facts could not be read",
            )
        })?;
        facts
            .into_iter()
            .map(|event| {
                Ok(KernelToolRuntimeEvent {
                    source_sequence: event.sequence,
                    fact_json: serde_json::to_string(&event.fact).map_err(|_| {
                        KernelFailure::new(
                            "TOOL_FACT_ENCODING_FAILED",
                            "Core execution fact could not be encoded",
                        )
                    })?,
                })
            })
            .collect()
    }
}
