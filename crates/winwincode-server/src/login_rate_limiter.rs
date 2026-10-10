// SPDX-License-Identifier: Apache-2.0

//! In-memory login failure rate limiting.
//!
//! Failures are counted per (normalized username, client IP) pair inside a
//! fixed window. Once the failure budget is exhausted, further login attempts
//! for that pair are rejected with an explicit rate-limit error until the
//! window passes. A successful login clears the pair's counters.

use std::collections::HashMap;
use std::sync::Mutex;

/// Failures allowed inside one window for one (username, client) pair.
pub(crate) const MAX_LOGIN_FAILURES: u32 = 5;
/// Fixed failure-counting window.
pub(crate) const LOGIN_FAILURE_WINDOW_MILLIS: i64 = 15 * 60 * 1000;
/// Upper bound on tracked pairs so spraying many usernames/IPs cannot grow
/// memory without limit. Expired windows are pruned first; if every tracked
/// pair is still live, the oldest window is evicted.
pub(crate) const MAX_TRACKED_PAIRS: usize = 65_536;

#[derive(Clone, Eq, Hash, PartialEq)]
struct AttemptKey {
    client: String,
    normalized_username: String,
}

struct AttemptEntry {
    failures: u32,
    window_started_millis: i64,
}

/// Counts login failures per (normalized username, client IP) pair.
#[derive(Default)]
pub(crate) struct LoginRateLimiter {
    entries: Mutex<HashMap<AttemptKey, AttemptEntry>>,
}

impl LoginRateLimiter {
    /// Reports whether the pair is currently locked out.
    #[must_use]
    pub(crate) fn rejected(&self, client: &str, normalized_username: &str, now: i64) -> bool {
        let key = AttemptKey {
            client: client.to_owned(),
            normalized_username: normalized_username.to_owned(),
        };
        let Ok(entries) = self.entries.lock() else {
            // Fail closed: a poisoned lock keeps the lockout active.
            return true;
        };
        entries.get(&key).is_some_and(|entry| {
            entry.failures >= MAX_LOGIN_FAILURES
                && now.saturating_sub(entry.window_started_millis) < LOGIN_FAILURE_WINDOW_MILLIS
        })
    }

    /// Records one failed login for the pair.
    pub(crate) fn record_failure(&self, client: &str, normalized_username: &str, now: i64) {
        let key = AttemptKey {
            client: client.to_owned(),
            normalized_username: normalized_username.to_owned(),
        };
        let Ok(mut entries) = self.entries.lock() else {
            return;
        };
        if !entries.contains_key(&key) && entries.len() >= MAX_TRACKED_PAIRS {
            entries.retain(|_, entry| {
                now.saturating_sub(entry.window_started_millis) < LOGIN_FAILURE_WINDOW_MILLIS
            });
            if entries.len() >= MAX_TRACKED_PAIRS
                && let Some(oldest) = entries
                    .iter()
                    .min_by_key(|(_, entry)| entry.window_started_millis)
                    .map(|(key, _)| key.clone())
            {
                entries.remove(&oldest);
            }
        }
        let entry = entries.entry(key).or_insert(AttemptEntry {
            failures: 0,
            window_started_millis: now,
        });
        if now.saturating_sub(entry.window_started_millis) >= LOGIN_FAILURE_WINDOW_MILLIS {
            entry.window_started_millis = now;
            entry.failures = 0;
        }
        entry.failures = entry.failures.saturating_add(1);
    }

    /// Clears the pair's counters after one successful login.
    pub(crate) fn clear(&self, client: &str, normalized_username: &str) {
        let key = AttemptKey {
            client: client.to_owned(),
            normalized_username: normalized_username.to_owned(),
        };
        if let Ok(mut entries) = self.entries.lock() {
            entries.remove(&key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locks_out_after_the_failure_budget_and_recovers_after_the_window() {
        let limiter = LoginRateLimiter::default();
        for attempt in 0..i64::from(MAX_LOGIN_FAILURES) {
            assert!(
                !limiter.rejected("203.0.113.9", "wen", attempt * 1000),
                "attempt {attempt} before the budget"
            );
            limiter.record_failure("203.0.113.9", "wen", attempt * 1000);
        }
        assert!(limiter.rejected("203.0.113.9", "wen", 6_000));

        // Other usernames and clients stay unaffected.
        assert!(!limiter.rejected("203.0.113.9", "ada", 6_000));
        assert!(!limiter.rejected("198.51.100.4", "wen", 6_000));

        // A passed window resets the counter.
        assert!(!limiter.rejected("203.0.113.9", "wen", LOGIN_FAILURE_WINDOW_MILLIS + 6_000));
    }

    #[test]
    fn tracked_pairs_stay_bounded_and_expired_windows_are_pruned() {
        let limiter = LoginRateLimiter::default();
        for index in 0..MAX_TRACKED_PAIRS {
            limiter.record_failure(&format!("client-{index}"), "wen", 0);
        }
        // A new pair at capacity evicts one live entry, never grows past the cap.
        limiter.record_failure("fresh", "wen", 1);
        assert_eq!(limiter.entries.lock().unwrap().len(), MAX_TRACKED_PAIRS);
        // Once the old windows expire, the next insert prunes them all.
        limiter.record_failure("later", "wen", LOGIN_FAILURE_WINDOW_MILLIS + 2);
        assert_eq!(limiter.entries.lock().unwrap().len(), 1);
        // Lockout behavior for live pairs is unchanged.
        for attempt in 0..i64::from(MAX_LOGIN_FAILURES) {
            limiter.record_failure("203.0.113.9", "wen", LOGIN_FAILURE_WINDOW_MILLIS + attempt);
        }
        assert!(limiter.rejected("203.0.113.9", "wen", LOGIN_FAILURE_WINDOW_MILLIS + 10));
    }

    #[test]
    fn one_success_clears_the_pair_counters() {
        let limiter = LoginRateLimiter::default();
        for attempt in 0..i64::from(MAX_LOGIN_FAILURES) {
            limiter.record_failure("203.0.113.9", "wen", attempt * 1000);
        }
        limiter.clear("203.0.113.9", "wen");
        assert!(!limiter.rejected("203.0.113.9", "wen", 1_000));
    }
}
