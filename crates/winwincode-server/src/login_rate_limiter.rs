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
//! is still inside an active lockout, or that belongs to an attempt still in
//! flight, is never evicted, so filling the table cannot clear a lockout.
//!
//! Admission is atomic: [`LoginRateLimiter::admit`] checks lockouts and
//! reserves a tracked slot in both tables under one lock before any password
//! work. When a new key cannot be tracked because a table is full of live
//! lockouts, the attempt is rejected up front instead of running unaccounted.

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
    /// Admitted attempts that have not yet recorded their result.
    in_flight: u32,
}

impl AttemptEntry {
    fn fresh(now: i64) -> Self {
        Self {
            failures: 0,
            window_started_millis: now,
            in_flight: 0,
        }
    }

    fn live(self, now: i64) -> bool {
        now.saturating_sub(self.window_started_millis) < LOGIN_FAILURE_WINDOW_MILLIS
    }

    fn locked(self, now: i64, limit: u32) -> bool {
        self.failures >= limit && self.live(now)
    }

    /// Locked or reserved by an in-flight attempt: never evicted while live.
    fn protected(self, now: i64, limit: u32) -> bool {
        self.live(now) && (self.failures >= limit || self.in_flight > 0)
    }

    fn roll_window(&mut self, now: i64) {
        if !self.live(now) {
            self.window_started_millis = now;
            self.failures = 0;
        }
    }
}

/// One bounded failure table.
struct BoundedTable<K> {
    entries: HashMap<K, AttemptEntry>,
    capacity: usize,
    limit: u32,
    /// When every entry is protected, no new key is admitted until this
    /// instant (the earliest window expiry); avoids rescanning per request.
    /// Cleared whenever an entry stops being protected.
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

    /// Non-mutating hint: a missing key is known to be untrackable.
    fn known_saturated_for(&self, key: &K, now: i64) -> bool {
        !self.entries.contains_key(key)
            && self.entries.len() >= self.capacity
            && self.saturated_until.is_some_and(|until| now < until)
    }

    /// Makes room for one new key. Returns false when every tracked entry is
    /// protected, in which case the new key cannot be tracked.
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
            // Then the oldest entries that are neither locked nor in flight.
            let limit = self.limit;
            let mut candidates: Vec<(i64, K)> = self
                .entries
                .iter()
                .filter(|(_, entry)| !entry.protected(now, limit))
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
        // Everything left is protected: keep it all, admit nothing new.
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

    /// Reserves one in-flight attempt for `key`, inserting it if needed.
    /// Returns false when the key is locked or cannot be tracked.
    fn reserve(&mut self, key: &K, now: i64) -> bool {
        if let Some(entry) = self.entries.get_mut(key) {
            if entry.locked(now, self.limit) {
                return false;
            }
            entry.roll_window(now);
            entry.in_flight = entry.in_flight.saturating_add(1);
            return true;
        }
        if !self.make_room(now) {
            return false;
        }
        let mut entry = AttemptEntry::fresh(now);
        entry.in_flight = 1;
        self.entries.insert(key.clone(), entry);
        true
    }

    /// Undoes a reservation without recording a failure.
    fn release(&mut self, key: &K) {
        if let Some(entry) = self.entries.get_mut(key) {
            entry.in_flight = entry.in_flight.saturating_sub(1);
            // An entry with no failures and no attempts left carries no
            // information; drop it so reservations never leak slots.
            if entry.failures == 0 && entry.in_flight == 0 {
                self.entries.remove(key);
            }
        }
        self.saturated_until = None;
    }

    /// Records one failure, consuming one in-flight reservation if present.
    fn record(&mut self, key: K, now: i64) {
        if let Some(entry) = self.entries.get_mut(&key) {
            entry.in_flight = entry.in_flight.saturating_sub(1);
            entry.roll_window(now);
            entry.failures = entry.failures.saturating_add(1);
            if !entry.protected(now, self.limit) {
                self.saturated_until = None;
            }
            return;
        }
        if !self.make_room(now) {
            return;
        }
        let mut entry = AttemptEntry::fresh(now);
        entry.failures = 1;
        self.entries.insert(key, entry);
    }

    fn remove(&mut self, key: &K) {
        if self.entries.remove(key).is_some() {
            self.saturated_until = None;
        }
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

fn attempt_key(client: &str, normalized_username: &str) -> AttemptKey {
    AttemptKey {
        client: client.to_owned(),
        normalized_username: normalized_username.to_owned(),
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

    /// Cheap, non-mutating pre-check: reports whether the pair or client is
    /// locked out, or a new key is already known to be untrackable. The
    /// authoritative decision is [`Self::admit`].
    #[must_use]
    pub(crate) fn rejected(&self, client: &str, normalized_username: &str, now: i64) -> bool {
        let key = attempt_key(client, normalized_username);
        let client = client.to_owned();
        let Ok(tables) = self.tables.lock() else {
            // Fail closed: a poisoned lock keeps the lockout active.
            return true;
        };
        tables.pairs.locked(&key, now)
            || tables.clients.locked(&client, now)
            || tables.pairs.known_saturated_for(&key, now)
            || tables.clients.known_saturated_for(&client, now)
    }

    /// Atomically admits one password attempt: rejects locked pairs/clients
    /// and keys that cannot be tracked, otherwise reserves an in-flight slot
    /// in both tables so the result is guaranteed to be recorded. Every
    /// admitted attempt must end in [`Self::record_failure`], [`Self::clear`]
    /// or [`Self::release`].
    #[must_use]
    pub(crate) fn admit(&self, client: &str, normalized_username: &str, now: i64) -> bool {
        let key = attempt_key(client, normalized_username);
        let client = client.to_owned();
        let Ok(mut tables) = self.tables.lock() else {
            return false;
        };
        if tables.pairs.locked(&key, now) || tables.clients.locked(&client, now) {
            return false;
        }
        if !tables.pairs.reserve(&key, now) {
            return false;
        }
        if !tables.clients.reserve(&client, now) {
            tables.pairs.release(&key);
            return false;
        }
        true
    }

    /// Ends an admitted attempt that produced neither success nor a
    /// credential failure (for example a storage error).
    pub(crate) fn release(&self, client: &str, normalized_username: &str) {
        let key = attempt_key(client, normalized_username);
        if let Ok(mut tables) = self.tables.lock() {
            tables.pairs.release(&key);
            tables.clients.release(&client.to_owned());
        }
    }

    /// Records one failed login for the pair and its client IP.
    pub(crate) fn record_failure(&self, client: &str, normalized_username: &str, now: i64) {
        let key = attempt_key(client, normalized_username);
        let Ok(mut tables) = self.tables.lock() else {
            return;
        };
        tables.pairs.record(key, now);
        tables.clients.record(client.to_owned(), now);
    }

    /// Clears the pair's counters after one successful login. The client IP
    /// failure count is kept so a valid account cannot launder a spray; only
    /// its in-flight reservation is released.
    pub(crate) fn clear(&self, client: &str, normalized_username: &str) {
        let key = attempt_key(client, normalized_username);
        if let Ok(mut tables) = self.tables.lock() {
            tables.pairs.remove(&key);
            tables.clients.release(&client.to_owned());
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

    /// One login attempt as the server performs it: admission first, then a
    /// failed password check. Returns whether password work ran.
    fn failed_attempt(limiter: &LoginRateLimiter, client: &str, user: &str, now: i64) -> bool {
        if limiter.rejected(client, user, now) || !limiter.admit(client, user, now) {
            return false;
        }
        limiter.record_failure(client, user, now);
        true
    }

    fn lock_pairs(limiter: &LoginRateLimiter, count: usize, now: i64) {
        for index in 0..count {
            let client = format!("198.51.100.{index}");
            for _ in 0..MAX_LOGIN_FAILURES {
                assert!(failed_attempt(limiter, &client, "wen", now));
            }
            assert!(limiter.rejected(&client, "wen", now));
        }
    }

    fn lengths(limiter: &LoginRateLimiter) -> (usize, usize) {
        let tables = limiter.tables.lock().unwrap();
        (tables.pairs.entries.len(), tables.clients.entries.len())
    }

    #[test]
    fn untrackable_new_pair_is_rejected_before_password_work() {
        let limiter = LoginRateLimiter::with_capacity(8, 1024);
        lock_pairs(&limiter, 8, 0);
        // First saturation (no cached state yet): the fresh pair is refused,
        // never silently degraded to the 50-failure client budget.
        let ran = (0..100)
            .filter(|_| failed_attempt(&limiter, "192.0.2.1", "wen", 10))
            .count();
        assert_eq!(ran, 0);
        assert!(limiter.rejected("192.0.2.1", "wen", 11));
        assert_eq!(lengths(&limiter).0, 8);
        for index in 0..8 {
            assert!(limiter.rejected(&format!("198.51.100.{index}"), "wen", 20));
        }
    }

    #[test]
    fn both_tables_saturated_never_run_unrecorded_attempts() {
        let limiter = LoginRateLimiter::with_capacity(8, 8);
        lock_pairs(&limiter, 8, 0);
        // Existing clients try new usernames: those pairs cannot be tracked,
        // so the attempts are refused rather than counted only per client.
        for index in 0..8 {
            for attempt in 0..45 {
                assert!(!failed_attempt(
                    &limiter,
                    &format!("198.51.100.{index}"),
                    &format!("user-{attempt}"),
                    5
                ));
            }
        }
        let ran = (0..100)
            .filter(|_| failed_attempt(&limiter, "192.0.2.1", "fresh", 10))
            .count();
        assert_eq!(ran, 0, "a fresh client cannot keep guessing unrecorded");
        assert_eq!(lengths(&limiter), (8, 8));
        for index in 0..8 {
            assert!(limiter.rejected(&format!("198.51.100.{index}"), "wen", 20));
        }
    }

    #[test]
    fn full_client_table_rejects_untrackable_clients() {
        let limiter = LoginRateLimiter::with_capacity(1024, 8);
        // Eight clients exhaust their client-wide budget across usernames.
        for index in 0..8 {
            let client = format!("198.51.100.{index}");
            for user in 0..MAX_CLIENT_LOGIN_FAILURES {
                assert!(failed_attempt(&limiter, &client, &format!("u{user}"), 0));
            }
            assert!(limiter.rejected(&client, "anyone", 1));
        }
        assert!(!failed_attempt(&limiter, "192.0.2.1", "wen", 10));
        assert_eq!(lengths(&limiter).1, 8);
        // The pair reservation was rolled back, not leaked.
        assert!(
            !limiter
                .tables
                .lock()
                .unwrap()
                .pairs
                .entries
                .contains_key(&attempt_key("192.0.2.1", "wen"))
        );
    }

    #[test]
    fn capacity_recovers_after_expiry_and_clear() {
        let limiter = LoginRateLimiter::with_capacity(8, 1024);
        lock_pairs(&limiter, 8, 0);
        assert!(!failed_attempt(&limiter, "192.0.2.1", "wen", 10));
        // A successful login frees one slot immediately.
        limiter.clear("198.51.100.0", "wen");
        assert!(failed_attempt(&limiter, "192.0.2.1", "wen", 20));
        // After the window passes, fresh keys count and lock normally.
        let later = LOGIN_FAILURE_WINDOW_MILLIS + 30;
        for _ in 0..MAX_LOGIN_FAILURES {
            assert!(failed_attempt(&limiter, "192.0.2.9", "ada", later));
        }
        assert!(!failed_attempt(&limiter, "192.0.2.9", "ada", later + 1));
        assert!(lengths(&limiter).0 <= 8);
    }

    #[test]
    fn in_flight_reservation_is_never_evicted_or_dropped() {
        let limiter = LoginRateLimiter::with_capacity(8, 1024);
        assert!(limiter.admit("192.0.2.1", "wen", 0));
        lock_pairs(&limiter, 7, 0);
        // The table is full of lockouts plus one reservation: nothing new fits.
        assert!(!limiter.admit("192.0.2.2", "wen", 1));
        // The admitted attempt's failure is still recorded.
        limiter.record_failure("192.0.2.1", "wen", 2);
        let tables = limiter.tables.lock().unwrap();
        let entry = tables.pairs.entries[&attempt_key("192.0.2.1", "wen")];
        assert_eq!((entry.failures, entry.in_flight), (1, 0));
    }

    #[test]
    fn released_attempt_frees_its_reservation() {
        let limiter = LoginRateLimiter::with_capacity(8, 1024);
        assert!(limiter.admit("192.0.2.1", "wen", 0));
        limiter.release("192.0.2.1", "wen");
        let tables = limiter.tables.lock().unwrap();
        assert!(tables.pairs.entries.is_empty());
        assert!(tables.clients.entries.is_empty());
    }
}
