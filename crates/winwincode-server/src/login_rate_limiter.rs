// SPDX-License-Identifier: Apache-2.0

//! In-memory login failure rate limiting.
//!
//! Failures are counted per (normalized username, client IP) pair inside a
//! fixed window. Once the failure budget is exhausted, further login attempts
//! for that pair are rejected with an explicit rate-limit error until the
//! window passes. A successful login clears the pair's counters.
//!
//! Failures are additionally counted per client IP across all usernames so one
//! address cannot spray many usernames. Both tables are bounded; an entry that
//! is still inside an active lockout is never evicted, so filling the table
//! cannot clear an existing lockout.

use std::collections::HashMap;
use std::hash::Hash;
use std::sync::Mutex;

/// Failures allowed inside one window for one (username, client) pair.
pub(crate) const MAX_LOGIN_FAILURES: u32 = 5;
/// Failures allowed inside one window for one client IP across all usernames.
pub(crate) const MAX_CLIENT_LOGIN_FAILURES: u32 = 50;
/// Fixed failure-counting window.
pub(crate) const LOGIN_FAILURE_WINDOW_MILLIS: i64 = 15 * 60 * 1000;
/// Upper bound on tracked pairs so spraying many usernames/IPs cannot grow
/// memory without limit.
pub(crate) const MAX_TRACKED_PAIRS: usize = 65_536;
/// Upper bound on tracked client IPs.
pub(crate) const MAX_TRACKED_CLIENTS: usize = 65_536;
/// Fraction of a full table freed by one compaction pass (1/N), so the O(n)
/// scan is amortized over many later inserts.
const EVICTION_BATCH_DIVISOR: usize = 16;

#[derive(Clone, Eq, Hash, PartialEq)]
struct AttemptKey {
    client: String,
    normalized_username: String,
}

#[derive(Clone, Copy)]
struct AttemptEntry {
    failures: u32,
    window_started_millis: i64,
}

impl AttemptEntry {
    fn live(self, now: i64) -> bool {
        now.saturating_sub(self.window_started_millis) < LOGIN_FAILURE_WINDOW_MILLIS
    }

    fn locked(self, now: i64, limit: u32) -> bool {
        self.failures >= limit && self.live(now)
    }

    fn record(&mut self, now: i64) {
        if !self.live(now) {
            self.window_started_millis = now;
            self.failures = 0;
        }
        self.failures = self.failures.saturating_add(1);
    }
}

/// One bounded failure table.
struct BoundedTable<K> {
    entries: HashMap<K, AttemptEntry>,
    capacity: usize,
    limit: u32,
    /// When every entry is locked, no new key is admitted until this instant
    /// (the earliest lockout expiry); avoids rescanning on every request.
    saturated_until: Option<i64>,
}

impl<K: Eq + Hash + Clone> BoundedTable<K> {
    fn new(capacity: usize, limit: u32) -> Self {
        Self {
            entries: HashMap::new(),
            capacity,
            limit,
            saturated_until: None,
        }
    }

    fn locked(&self, key: &K, now: i64) -> bool {
        self.entries
            .get(key)
            .is_some_and(|entry| entry.locked(now, self.limit))
    }

    /// Makes room for one new key. Returns false when every tracked entry is
    /// still locked out, in which case the new key must not be recorded.
    fn make_room(&mut self, now: i64) -> bool {
        if self.entries.len() < self.capacity {
            return true;
        }
        if self.saturated_until.is_some_and(|until| now < until) {
            return false;
        }
        self.saturated_until = None;
        // One O(n) pass: drop expired windows first.
        self.entries.retain(|_, entry| entry.live(now));
        let target = self.capacity - (self.capacity / EVICTION_BATCH_DIVISOR).max(1);
        if self.entries.len() > target {
            // Then the oldest entries that are not locked out, in one batch.
            let limit = self.limit;
            let mut candidates: Vec<(i64, K)> = self
                .entries
                .iter()
                .filter(|(_, entry)| !entry.locked(now, limit))
                .map(|(key, entry)| (entry.window_started_millis, key.clone()))
                .collect();
            let excess = self.entries.len() - target;
            if candidates.len() > excess {
                candidates.select_nth_unstable_by_key(excess, |(started, _)| *started);
                candidates.truncate(excess);
            }
            for (_, key) in candidates {
                self.entries.remove(&key);
            }
        }
        if self.entries.len() < self.capacity {
            return true;
        }
        // Everything left is locked: keep all lockouts, admit nothing new.
        self.saturated_until = self
            .entries
            .values()
            .map(|entry| {
                entry
                    .window_started_millis
                    .saturating_add(LOGIN_FAILURE_WINDOW_MILLIS)
            })
            .min();
        false
    }

    fn record(&mut self, key: K, now: i64) {
        if let Some(entry) = self.entries.get_mut(&key) {
            entry.record(now);
            return;
        }
        if !self.make_room(now) {
            return;
        }
        let mut entry = AttemptEntry {
            failures: 0,
            window_started_millis: now,
        };
        entry.record(now);
        self.entries.insert(key, entry);
    }
}

struct Tables {
    pairs: BoundedTable<AttemptKey>,
    clients: BoundedTable<String>,
}

/// Counts login failures per (normalized username, client IP) pair and per
/// client IP.
pub(crate) struct LoginRateLimiter {
    tables: Mutex<Tables>,
}

impl Default for LoginRateLimiter {
    fn default() -> Self {
        Self::with_capacity(MAX_TRACKED_PAIRS, MAX_TRACKED_CLIENTS)
    }
}

impl LoginRateLimiter {
    fn with_capacity(pairs: usize, clients: usize) -> Self {
        Self {
            tables: Mutex::new(Tables {
                pairs: BoundedTable::new(pairs, MAX_LOGIN_FAILURES),
                clients: BoundedTable::new(clients, MAX_CLIENT_LOGIN_FAILURES),
            }),
        }
    }

    /// Reports whether the pair or the client IP is currently locked out.
    #[must_use]
    pub(crate) fn rejected(&self, client: &str, normalized_username: &str, now: i64) -> bool {
        let key = AttemptKey {
            client: client.to_owned(),
            normalized_username: normalized_username.to_owned(),
        };
        let Ok(tables) = self.tables.lock() else {
            // Fail closed: a poisoned lock keeps the lockout active.
            return true;
        };
        tables.pairs.locked(&key, now) || tables.clients.locked(&client.to_owned(), now)
    }

    /// Records one failed login for the pair and its client IP.
    pub(crate) fn record_failure(&self, client: &str, normalized_username: &str, now: i64) {
        let key = AttemptKey {
            client: client.to_owned(),
            normalized_username: normalized_username.to_owned(),
        };
        let Ok(mut tables) = self.tables.lock() else {
            return;
        };
        tables.pairs.record(key, now);
        tables.clients.record(client.to_owned(), now);
    }

    /// Clears the pair's counters after one successful login. The client IP
    /// counter is kept so a valid account cannot launder a spray.
    pub(crate) fn clear(&self, client: &str, normalized_username: &str) {
        let key = AttemptKey {
            client: client.to_owned(),
            normalized_username: normalized_username.to_owned(),
        };
        if let Ok(mut tables) = self.tables.lock() {
            tables.pairs.entries.remove(&key);
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
        let limiter = LoginRateLimiter::with_capacity(64, 1024);
        for index in 0..64 {
            limiter.record_failure(&format!("client-{index}"), "wen", 0);
        }
        limiter.record_failure("fresh", "wen", 1);
        assert!(limiter.tables.lock().unwrap().pairs.entries.len() <= 64);
        // Once the old windows expire, the next compaction prunes them all.
        for index in 0..64 {
            limiter.record_failure(
                &format!("late-{index}"),
                "wen",
                LOGIN_FAILURE_WINDOW_MILLIS + 2,
            );
        }
        {
            let tables = limiter.tables.lock().unwrap();
            assert_eq!(tables.pairs.entries.len(), 64);
            assert!(
                tables
                    .pairs
                    .entries
                    .keys()
                    .all(|key| key.client.starts_with("late-"))
            );
        }
        for attempt in 0..i64::from(MAX_LOGIN_FAILURES) {
            limiter.record_failure("203.0.113.9", "wen", LOGIN_FAILURE_WINDOW_MILLIS + attempt);
        }
        assert!(limiter.rejected("203.0.113.9", "wen", LOGIN_FAILURE_WINDOW_MILLIS + 10));
    }

    #[test]
    fn full_table_keeps_locked_pairs_rejected() {
        let limiter = LoginRateLimiter::with_capacity(8, 1024);
        // Fill the table with locked pairs.
        for index in 0..8 {
            let client = format!("198.51.100.{index}");
            for attempt in 0..i64::from(MAX_LOGIN_FAILURES) {
                limiter.record_failure(&client, "wen", attempt);
            }
        }
        // New pairs from other clients cannot evict any lockout.
        for index in 0..100 {
            limiter.record_failure(&format!("192.0.2.{index}"), "ada", 10);
        }
        for index in 0..8 {
            assert!(limiter.rejected(&format!("198.51.100.{index}"), "wen", 20));
        }
        assert_eq!(limiter.tables.lock().unwrap().pairs.entries.len(), 8);
    }

    #[test]
    fn username_spray_from_one_client_cannot_unlock_its_lockout() {
        let limiter = LoginRateLimiter::with_capacity(16, 1024);
        for attempt in 0..i64::from(MAX_LOGIN_FAILURES) {
            limiter.record_failure("203.0.113.9", "wen", attempt);
        }
        assert!(limiter.rejected("203.0.113.9", "wen", 10));
        for index in 0..1000 {
            limiter.record_failure("203.0.113.9", &format!("user-{index}"), 10 + index);
        }
        assert!(limiter.rejected("203.0.113.9", "wen", 2_000));
        // The client-wide budget now rejects every username from that IP.
        assert!(limiter.rejected("203.0.113.9", "someone-new", 2_000));
        // Other clients are unaffected.
        assert!(!limiter.rejected("198.51.100.4", "someone-new", 2_000));
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
