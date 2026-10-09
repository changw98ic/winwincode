// SPDX-License-Identifier: Apache-2.0

//! Shared bounded request retries and cancellable connection recovery. Unknown Provider acceptance does not prohibit retry.
#[cfg(feature = "codex")]
pub mod codex;
mod diagnostic;
pub mod http;
#[cfg(feature = "journal")]
pub mod journal;
mod policy;
pub mod transport;
pub use diagnostic::*;
pub use policy::*;

use sha2::{Digest, Sha256};
use std::time::{Duration, Instant};

pub trait RetryFailure {
    fn retryable(&self) -> bool;
    fn retry_after(&self) -> Option<Duration> {
        None
    }
    fn wait_for_connection(&self) -> bool {
        false
    }
    fn network_failure(&self) -> NetworkFailure {
        let mut failure = NetworkFailure::new(
            if self.wait_for_connection() {
                ErrorKind::ConnectionUnavailable
            } else if self.retryable() {
                ErrorKind::TransportInterrupted
            } else {
                // An explicit permanent error must not become retryable when
                // upstream protocol failures enter the shared retry policy.
                ErrorKind::RequestInvalid
            },
            if self.wait_for_connection() {
                Acceptance::NotSent
            } else {
                Acceptance::Unknown
            },
            Phase::ResponseHeaders,
        );
        failure.retry_after_ms = self.retry_after().map(duration_millis);
        failure
    }
}

impl RetryFailure for NetworkFailure {
    fn retryable(&self) -> bool {
        (*self).retryable()
    }
    fn retry_after(&self) -> Option<Duration> {
        self.retry_after_ms.map(Duration::from_millis)
    }
    fn wait_for_connection(&self) -> bool {
        self.acceptance == Acceptance::NotSent
            && matches!(
                self.kind,
                ErrorKind::ConnectionUnavailable | ErrorKind::Timeout
            )
    }
    fn network_failure(&self) -> NetworkFailure {
        *self
    }
}

#[derive(Clone, Copy)]
pub struct RequestRetry {
    max_attempts: u32,
    jitter_key: [u8; 32],
    replay: Replay,
}

pub struct RequestRetryState {
    policy: RequestRetry,
    attempt: u32,
    connections: u32,
}

impl RequestRetryState {
    pub fn attempt(&self) -> u32 {
        self.attempt
    }

    pub fn delay_after(&mut self, error: &impl RetryFailure) -> Option<Duration> {
        self.decision_after(error).immediate_delay()
    }

    /// Queue owners persist this decision instead of adding another retry loop.
    pub fn decision_after(&mut self, error: &impl RetryFailure) -> RetryDecision {
        if error.wait_for_connection() {
            self.connections = self.connections.saturating_add(1);
            return self.policy.decide(self.attempt, self.connections, error);
        }
        let decision = self.policy.decide(self.attempt, 0, error);
        if matches!(
            decision,
            RetryDecision::RetryAfter(_) | RetryDecision::DeferredUntil(_)
        ) {
            self.attempt = self.attempt.saturating_add(1);
        }
        self.connections = 0;
        decision
    }
}

impl RequestRetry {
    pub fn new(max_attempts: u32, key: &[u8]) -> Self {
        let mut jitter_key: [u8; 32] = Sha256::digest(key).into();
        // Independent operations should not wake every request to one Provider
        // at the same time. A missing entropy source still permits safe retries.
        let _ = getrandom::fill(&mut jitter_key);
        Self {
            max_attempts: max_attempts.max(1),
            jitter_key,
            replay: Replay::RetryInference,
        }
    }

    #[must_use]
    pub fn for_replay(mut self, replay: Replay) -> Self {
        self.replay = replay;
        self
    }

    pub fn state(self) -> RequestRetryState {
        RequestRetryState {
            policy: self,
            attempt: 1,
            connections: 0,
        }
    }

    pub fn delay_after(&self, attempt: u32, error: &impl RetryFailure) -> Option<Duration> {
        self.decide(attempt, attempt, error).immediate_delay()
    }

    pub fn decide(
        &self,
        attempt: u32,
        connection_attempt: u32,
        error: &impl RetryFailure,
    ) -> RetryDecision {
        let mut hash = Sha256::new();
        hash.update(self.jitter_key);
        hash.update(attempt.to_le_bytes());
        decide(
            error.network_failure(),
            self.replay,
            attempt,
            connection_attempt,
            self.max_attempts,
            u64::from(hash.finalize()[0]),
        )
    }

    /// Runs one logical request and retains each attempt before rescheduling.
    ///
    /// # Errors
    /// Returns an operation failure, an authority failure or a retention failure.
    pub fn run_blocking<T, E: RetryFailure>(
        &self,
        can_start: impl Fn() -> bool,
        stopped: impl Fn() -> E,
        mut operation: impl FnMut(u32) -> Result<T, E>,
        mut retain: impl FnMut(u32, &Result<T, E>) -> Result<(), E>,
    ) -> Result<T, E> {
        let mut state = self.state();
        loop {
            if !can_start() {
                return Err(stopped());
            }
            let result = operation(state.attempt());
            retain(state.attempt(), &result)?;
            let error = match result {
                Ok(value) => return Ok(value),
                Err(error) => error,
            };
            let Some(delay) = state.delay_after(&error) else {
                return Err(error);
            };
            let deadline = Instant::now() + delay;
            while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
                if !can_start() {
                    return Err(stopped());
                }
                std::thread::sleep(
                    remaining.min(Duration::from_millis(defaults().authority_check_ms)),
                );
            }
        }
    }

    pub fn connection_delay(attempt: u32) -> Duration {
        exponential_delay(
            attempt,
            Duration::from_millis(defaults().initial_delay_ms),
            Duration::from_millis(defaults().max_connection_delay_ms),
        )
    }

    pub async fn wait_async(delay: Duration, can_start: impl Fn() -> bool) -> bool {
        let deadline = tokio::time::Instant::now() + delay;
        loop {
            if !can_start() {
                return false;
            }
            let Some(remaining) = deadline.checked_duration_since(tokio::time::Instant::now())
            else {
                return true;
            };
            tokio::time::sleep(remaining.min(Duration::from_millis(defaults().authority_check_ms)))
                .await;
        }
    }

    /// One layer owns retries. Dropping an active future interrupts its I/O on authority loss.
    ///
    /// # Errors
    /// Returns an operation failure, an authority failure or a retention failure.
    pub async fn run_async<T, E: RetryFailure, F: Future<Output = Result<T, E>>>(
        &self,
        can_start: impl Fn() -> bool,
        stopped: impl Fn() -> E,
        mut operation: impl FnMut(u32) -> F,
        mut retain: impl FnMut(u32, &Result<T, E>) -> Result<(), E>,
    ) -> Result<T, E> {
        let mut state = self.state();
        loop {
            if !can_start() {
                return Err(stopped());
            }
            let mut pending = Box::pin(operation(state.attempt()));
            let result = loop {
                if !can_start() {
                    return Err(stopped());
                }
                if let Ok(result) = tokio::time::timeout(
                    Duration::from_millis(defaults().authority_check_ms),
                    pending.as_mut(),
                )
                .await
                {
                    break result;
                }
            };
            retain(state.attempt(), &result)?;
            let error = match result {
                Ok(value) => return Ok(value),
                Err(error) => error,
            };
            let Some(delay) = state.delay_after(&error) else {
                return Err(error);
            };
            if !Self::wait_async(delay, &can_start).await {
                return Err(stopped());
            }
        }
    }
}

impl RetryDecision {
    pub const fn immediate_delay(self) -> Option<Duration> {
        match self {
            Self::RetryAfter(delay) => Some(delay),
            _ => None,
        }
    }
}

pub fn retry_after_delay(value: &str, now: time::OffsetDateTime) -> Option<Duration> {
    let now = std::time::UNIX_EPOCH.checked_add(Duration::from_secs(
        u64::try_from(now.unix_timestamp()).ok()?,
    ))?;
    retry_after(value, now)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn explicit_permanent_errors_do_not_gain_protocol_retries() {
        struct Permanent;
        impl RetryFailure for Permanent {
            fn retryable(&self) -> bool {
                false
            }
        }
        assert_eq!(
            RequestRetry::new(4, b"permanent")
                .state()
                .decision_after(&Permanent),
            RetryDecision::Stop
        );
    }

    #[test]
    fn shared_policy_vectors_match_runtime_decisions() {
        let vectors: serde_json::Value = serde_json::from_str(include_str!(
            "../../../tests/fixtures/network-request-policy.v1.json"
        ))
        .unwrap();
        for vector in vectors.as_array().unwrap() {
            let failure: NetworkFailure =
                serde_json::from_value(vector["failure"].clone()).unwrap();
            let replay: Replay = serde_json::from_value(vector["replay"].clone()).unwrap();
            let number = |key: &str| u32::try_from(vector[key].as_u64().unwrap()).unwrap();
            let decision = decide(
                failure,
                replay,
                number("attempt"),
                number("connectionAttempt"),
                number("maxAttempts"),
                0,
            );
            let (action, delay) = match decision {
                RetryDecision::RetryAfter(delay) => ("retry_after", Some(duration_millis(delay))),
                RetryDecision::DeferredUntil(delay) => {
                    ("deferred_until", Some(duration_millis(delay)))
                }
                RetryDecision::Reconcile => ("reconcile", None),
                RetryDecision::Stop => ("stop", None),
            };
            assert_eq!(action, vector["action"], "{}", vector["name"]);
            assert_eq!(delay, vector["delayMs"].as_u64(), "{}", vector["name"]);
        }
    }

    #[test]
    fn http_status_and_retry_after_classification_preserves_conditions() {
        for (status, kind) in [
            (408, ErrorKind::Timeout),
            (425, ErrorKind::ServerTransient),
            (429, ErrorKind::RateLimited),
            (503, ErrorKind::ServerTransient),
            (401, ErrorKind::Authentication),
            (403, ErrorKind::Authorization),
            (400, ErrorKind::RequestInvalid),
        ] {
            assert_eq!(NetworkFailure::http(status, None).kind, kind);
        }
        let now = std::time::UNIX_EPOCH + Duration::from_secs(1_000_000);
        assert_eq!(retry_after("12", now), Some(Duration::from_secs(12)));
        assert_eq!(
            retry_after(&httpdate::fmt_http_date(now + Duration::from_secs(30)), now),
            Some(Duration::from_secs(30))
        );
        assert!(retry_after("invalid", now).is_none());
    }

    #[test]
    fn active_async_io_stops_when_live_authority_is_revoked() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let started = Instant::now();
            let failure = RequestRetry::new(4, b"authority")
                .run_async(
                    || started.elapsed() < Duration::from_millis(20),
                    || {
                        NetworkFailure::new(
                            ErrorKind::AuthorityExpired,
                            Acceptance::Unknown,
                            Phase::ResponseHeaders,
                        )
                    },
                    |_| std::future::pending::<Result<(), NetworkFailure>>(),
                    |_, _| Ok(()),
                )
                .await
                .unwrap_err();
            assert_eq!(failure.kind, ErrorKind::AuthorityExpired);
            assert!(started.elapsed() < Duration::from_millis(500));
        });
    }
    struct Transient(Option<Duration>);
    impl RetryFailure for Transient {
        fn retryable(&self) -> bool {
            true
        }
        fn retry_after(&self) -> Option<Duration> {
            self.0
        }
    }
    #[test]
    fn connection_recovery_delay_caps_at_sixty_seconds() {
        for (attempt, seconds) in [(1, 5), (2, 10), (3, 20), (4, 40), (5, 60), (1000, 60)] {
            assert_eq!(
                RequestRetry::connection_delay(attempt),
                Duration::from_secs(seconds)
            );
        }
    }

    #[test]
    fn exponential_schedule_has_jitter_and_a_finite_limit() {
        let policy = RequestRetry::new(4, b"same logical request");
        for (attempt, seconds) in [(1, 5), (2, 10), (3, 20)] {
            let delay = policy.delay_after(attempt, &Transient(None)).unwrap();
            assert!(delay >= Duration::from_secs(seconds));
            assert!(delay <= Duration::from_secs(seconds) + Duration::from_millis(255));
            assert_eq!(
                delay,
                policy.delay_after(attempt, &Transient(None)).unwrap()
            );
        }
        assert!(policy.delay_after(4, &Transient(None)).is_none());
        assert!(
            policy
                .delay_after(1, &Transient(Some(Duration::from_secs(301))))
                .is_none()
        );
        assert!(
            policy
                .delay_after(1, &Transient(Some(Duration::ZERO)))
                .unwrap()
                <= Duration::from_secs(5) + Duration::from_millis(255)
        );
    }
}
