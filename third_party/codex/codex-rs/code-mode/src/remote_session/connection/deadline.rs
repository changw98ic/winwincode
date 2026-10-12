use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use tokio_util::sync::CancellationToken;
use tracing::warn;

use super::DEFAULT_HOST_WAIT_TRANSPORT_TIMEOUT;
use super::mark_connection_dead;

#[derive(Clone)]
pub(super) struct TransportDeadline {
    pub(super) alive: Arc<AtomicBool>,
    pub(super) failure: Arc<std::sync::Mutex<Option<String>>>,
    pub(super) cancellation: CancellationToken,
}

impl TransportDeadline {
    pub(super) async fn run<T>(
        &self,
        runtime_timeout: Duration,
        request_type: &str,
        request: impl Future<Output = Result<T, String>>,
    ) -> Result<T, String> {
        let deadline = runtime_timeout.saturating_add(DEFAULT_HOST_WAIT_TRANSPORT_TIMEOUT);
        match tokio::time::timeout(deadline, request).await {
            Ok(result) => result,
            Err(_) => {
                warn!(request_type, "code-mode host request exceeded its deadline");
                let reason =
                    format!("code-mode host timed out waiting for {request_type} response");
                mark_connection_dead(&self.alive, &self.failure, reason.clone());
                self.cancellation.cancel();
                Err(reason)
            }
        }
    }
}
