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
    /// Rejects an unavailable session or a Core storage/encoding failure.
    pub async fn tool_runtime_events(
        &self,
        session_id: &str,
        after: i64,
        limit: u32,
    ) -> KernelResult<Vec<KernelToolRuntimeEvent>> {
        let runtime = self.runtime().await?;
        let session = self.session(&runtime, session_id).await?;
        let facts = session
            .thread
            .tool_runtime_events(after, limit)
            .await
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
