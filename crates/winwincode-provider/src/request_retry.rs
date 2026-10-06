// SPDX-License-Identifier: Apache-2.0

//! Shared bounded request retries and cancellable connection recovery. Unknown Provider acceptance does not prohibit retry.
use sha2::{Digest, Sha256};
use std::time::{Duration, Instant};

pub(crate) trait RetryFailure {
    fn retryable(&self) -> bool;
    fn retry_after(&self) -> Option<Duration> {
        None
    }
    fn wait_for_connection(&self) -> bool {
        false
    }
}

#[derive(Clone, Copy)]
pub(crate) struct RequestRetry {
    max_attempts: u32,
    jitter_key: [u8; 32],
}

pub(crate) struct RequestRetryState {
    policy: RequestRetry,
    attempt: u32,
    connections: u32,
}

impl RequestRetryState {
    pub(crate) fn attempt(&self) -> u32 {
        self.attempt
    }

    pub(crate) fn delay_after(&mut self, error: &impl RetryFailure) -> Option<Duration> {
        if error.wait_for_connection() {
            self.connections = self.connections.saturating_add(1);
            return Some(RequestRetry::connection_delay(self.connections));
        }
        let delay = self.policy.delay_after(self.attempt, error)?;
        self.attempt += 1;
        self.connections = 0;
        Some(delay)
    }
}

impl RequestRetry {
    pub(crate) fn new(max_attempts: u32, key: &[u8]) -> Self {
        let mut jitter_key: [u8; 32] = Sha256::digest(key).into();
        // Independent operations should not wake every request to one Provider
        // at the same time. A missing entropy source still permits safe retries.
        let _ = getrandom::fill(&mut jitter_key);
        Self {
            max_attempts: max_attempts.max(1),
            jitter_key,
        }
    }

    pub(crate) fn state(self) -> RequestRetryState {
        RequestRetryState {
            policy: self,
            attempt: 1,
            connections: 0,
        }
    }

    pub(crate) fn delay_after(&self, attempt: u32, error: &impl RetryFailure) -> Option<Duration> {
        if attempt >= self.max_attempts || !error.retryable() {
            return None;
        }
        let delay = error.retry_after().unwrap_or_else(|| {
            Duration::from_secs(
                5_u64.saturating_mul(2_u64.saturating_pow(attempt.saturating_sub(1))),
            )
        });
        if delay > Duration::from_mins(5) {
            return None;
        }
        let mut hash = Sha256::new();
        hash.update(self.jitter_key);
        hash.update(attempt.to_le_bytes());
        Some(delay + Duration::from_millis(u64::from(hash.finalize()[0])))
    }

    pub(crate) fn run_blocking<T, E: RetryFailure>(
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
                std::thread::sleep(remaining.min(Duration::from_millis(100)));
            }
        }
    }

    pub(crate) fn connection_delay(attempt: u32) -> Duration {
        Duration::from_secs(
            5_u64
                .saturating_mul(2_u64.saturating_pow(attempt.saturating_sub(1)))
                .min(60),
        )
    }

    pub(crate) async fn wait_async(delay: Duration, can_start: impl Fn() -> bool) -> bool {
        let deadline = tokio::time::Instant::now() + delay;
        loop {
            if !can_start() {
                return false;
            }
            let Some(remaining) = deadline.checked_duration_since(tokio::time::Instant::now())
            else {
                return true;
            };
            tokio::time::sleep(remaining.min(Duration::from_millis(100))).await;
        }
    }
}

pub(crate) fn retry_after_delay(value: &str, now: time::OffsetDateTime) -> Option<Duration> {
    if let Ok(seconds) = value.trim().parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    let at =
        time::OffsetDateTime::parse(value.trim(), &time::format_description::well_known::Rfc2822)
            .ok()?;
    Some(Duration::from_secs(
        u64::try_from((at - now).whole_seconds()).unwrap_or(0),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
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
                <= Duration::from_millis(255)
        );
    }
}
