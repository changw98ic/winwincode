//! Embedders supply one request policy at the SDK retry ownership boundary.
use codex_exec_server::HttpNetworkFailure;
use std::{
    sync::{Arc, OnceLock},
    time::Duration,
};

#[derive(Clone, Debug, Default)]
pub struct NetworkRetryFacts {
    pub reconcile_first: bool,
    pub transport: Option<HttpNetworkFailure>,
    pub status: Option<u16>,
    pub retry_after: Option<String>,
}
pub type NetworkRetryPolicy =
    dyn Fn(u32, u32, &NetworkRetryFacts) -> Option<Duration> + Send + Sync;
static POLICY: OnceLock<Arc<NetworkRetryPolicy>> = OnceLock::new();

#[cfg(test)]
tokio::task_local! {
    pub(crate) static TEST_POLICY: Arc<NetworkRetryPolicy>;
}

pub(crate) fn policy_configured() -> bool {
    #[cfg(test)]
    if TEST_POLICY.try_with(|_| ()).is_ok() {
        return true;
    }
    POLICY.get().is_some()
}

/// Install before constructing MCP clients. Repeated initialization is harmless.
pub fn set_network_retry_policy(policy: Arc<NetworkRetryPolicy>) {
    let _ = POLICY.set(policy);
}

pub(crate) fn retry_delay(
    attempt: u32,
    connections: u32,
    facts: &NetworkRetryFacts,
) -> Option<Duration> {
    #[cfg(test)]
    if let Ok(delay) = TEST_POLICY.try_with(|policy| policy(attempt, connections, facts)) {
        return delay;
    }
    if let Some(policy) = POLICY.get() {
        policy(attempt, connections, facts)
    } else {
        [250, 1_000]
            .get(attempt.saturating_sub(1) as usize)
            .copied()
            .map(Duration::from_millis)
    }
}
