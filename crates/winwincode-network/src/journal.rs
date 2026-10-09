// SPDX-License-Identifier: Apache-2.0

//! Secret-free attempt facts and queue budgets survive process restarts.
use crate::{Acceptance, ErrorKind, NetworkFailure, Phase, Replay, RetryDecision};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime},
};

#[derive(Clone)]
pub struct RequestJournal(
    Arc<Mutex<Connection>>,
    Arc<Mutex<Option<(u64, Instant)>>>,
    u64,
);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QueuePermit {
    Ready(u64),
    Waiting(Duration, NetworkFailure),
    Stopped(NetworkFailure),
}

/// Only envelope cursor identities are retained; frame bodies and credentials stay in their source stores.
#[derive(Clone, Debug, Default, Deserialize, Serialize, Eq, PartialEq)]
pub struct EnvelopeCursor {
    pub last_sequence: u64,
    pub ack_sequence: u64,
    pub ack_ids: Vec<String>,
    pub input_digest: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct State {
    attempt: u32,
    connections: u32,
    sequence: u64,
    in_flight: bool,
    ready_at: u64,
    stopped: bool,
    failure: NetworkFailure,
    replay: Replay,
    max_attempts: u32,
}

fn storage() -> NetworkFailure {
    NetworkFailure::new(
        ErrorKind::StorageUnavailable,
        Acceptance::Unknown,
        Phase::Persist,
    )
    .with_diagnostic(crate::NetworkDiagnostic::new(
        crate::DiagnosticCode::Storage,
    ))
}
fn sql_int(value: u64) -> Result<i64, NetworkFailure> {
    i64::try_from(value).map_err(|_| storage())
}
fn key(value: &[u8]) -> String {
    format!("{:x}", Sha256::digest(value))
}

impl RequestJournal {
    /// Opens a private, additive journal. It stores hashes and bounded facts only.
    /// # Errors
    /// Rejects unsafe files, unavailable storage and invalid persisted state.
    pub fn open(path: &Path) -> Result<Self, NetworkFailure> {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(path) {
            Ok(file) => file.sync_all().map_err(|_| storage())?,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(_) => return Err(storage()),
        }
        let meta = std::fs::symlink_metadata(path).map_err(|_| storage())?;
        if !meta.is_file() {
            return Err(storage());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if meta.permissions().mode() & 0o077 != 0 {
                return Err(storage());
            }
        }
        let connection = Connection::open(path).map_err(|_| storage())?;
        connection
            .busy_timeout(Duration::from_secs(5))
            .map_err(|_| storage())?;
        connection.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;
             CREATE TABLE IF NOT EXISTS network_queue_state (
                 operation_hash TEXT PRIMARY KEY, state_json TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS network_envelope_cursors (operation_hash TEXT PRIMARY KEY, cursor_json TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS network_request_attempts (
                 operation_hash TEXT NOT NULL, sequence INTEGER NOT NULL,
                 started_ms INTEGER NOT NULL, finished_ms INTEGER,
                 outcome TEXT NOT NULL, failure_json TEXT,
                 PRIMARY KEY(operation_hash, sequence));"
        ).map_err(|_| storage())?;
        let persisted_clock: i64 = connection.query_row("SELECT COALESCE(MAX(MAX(started_ms,COALESCE(finished_ms,0))),0) FROM network_request_attempts", [], |row| row.get(0)).map_err(|_| storage())?;
        Ok(Self(
            Arc::new(Mutex::new(connection)),
            Arc::new(Mutex::new(None)),
            u64::try_from(persisted_clock).map_err(|_| storage())?,
        ))
    }

    /// Binds a logical request to stable non-secret envelope cursors across restart.
    /// # Errors
    /// Rejects malformed identifiers, changed frame identity and unavailable storage.
    pub fn bind_envelope(
        &self,
        operation: &[u8],
        cursor: &EnvelopeCursor,
    ) -> Result<EnvelopeCursor, NetworkFailure> {
        if cursor.ack_ids.len() > 4096
            || cursor.ack_ids.iter().any(|id| {
                id.len() > 200
                    || id.is_empty()
                    || !id
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
            })
            || cursor.input_digest.as_ref().is_some_and(|digest| {
                digest.len() != 64 || !digest.bytes().all(|b| b.is_ascii_hexdigit())
            })
        {
            return Err(storage());
        }
        let mut connection = self.0.lock().map_err(|_| storage())?;
        let transaction = connection.transaction().map_err(|_| storage())?;
        transaction
            .execute(
                "INSERT OR IGNORE INTO network_envelope_cursors VALUES(?1,?2)",
                params![
                    key(operation),
                    serde_json::to_string(cursor).map_err(|_| storage())?
                ],
            )
            .map_err(|_| storage())?;
        let json: String = transaction
            .query_row(
                "SELECT cursor_json FROM network_envelope_cursors WHERE operation_hash=?1",
                [key(operation)],
                |row| row.get(0),
            )
            .map_err(|_| storage())?;
        let retained: EnvelopeCursor = serde_json::from_str(&json).map_err(|_| storage())?;
        if retained.input_digest != cursor.input_digest {
            return Err(NetworkFailure::new(
                ErrorKind::IntegrityInvalid,
                Acceptance::NotSent,
                Phase::Decode,
            ));
        }
        transaction.commit().map_err(|_| storage())?;
        Ok(retained)
    }

    fn monotonic_now(&self, now: u64) -> Result<u64, NetworkFailure> {
        let mut clock = self.1.lock().map_err(|_| storage())?;
        let (origin, start) = clock.get_or_insert_with(|| (now.max(self.2), Instant::now()));
        let monotonic = origin.saturating_add(crate::duration_millis(start.elapsed()));
        if now > monotonic {
            *origin = now;
            *start = Instant::now();
        }
        Ok(now.max(monotonic))
    }

    /// Atomically records an unknown attempt before its external effect.
    /// A crash is counted as unknown acceptance on the next prepare.
    /// The calling queue must serialize each logical operation. A completed
    /// inference requires its retained completion; it cannot be charged again.
    /// # Errors
    /// Storage failures prohibit the external effect.
    pub fn prepare(
        &self,
        operation: &[u8],
        replay: Replay,
        max_attempts: u32,
        now: u64,
    ) -> Result<QueuePermit, NetworkFailure> {
        let now = self.monotonic_now(now)?;
        let operation = key(operation);
        let mut connection = self.0.lock().map_err(|_| storage())?;
        let transaction = connection.transaction().map_err(|_| storage())?;
        let encoded: Option<String> = transaction
            .query_row(
                "SELECT state_json FROM network_queue_state WHERE operation_hash=?1",
                [&operation],
                |row| row.get(0),
            )
            .optional()
            .map_err(|_| storage())?;
        if encoded.is_none()
            && replay == Replay::RetryInference
            && transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM network_request_attempts WHERE operation_hash=?1 AND outcome='completed')",
                [&operation], |row| row.get::<_, bool>(0),
            ).map_err(|_| storage())?
        {
            return Ok(QueuePermit::Stopped(NetworkFailure::new(
                ErrorKind::IntegrityInvalid, Acceptance::Unknown, Phase::Persist,
            )));
        }
        let unknown = NetworkFailure::new(
            ErrorKind::TransportInterrupted,
            Acceptance::Unknown,
            Phase::ResponseHeaders,
        );
        let mut state = encoded
            .map(|value| serde_json::from_str::<State>(&value).map_err(|_| storage()))
            .transpose()?
            .unwrap_or(State {
                attempt: 1,
                connections: 0,
                sequence: 0,
                in_flight: false,
                ready_at: 0,
                stopped: false,
                failure: unknown,
                replay,
                max_attempts,
            });
        if state.replay != replay || state.max_attempts != max_attempts {
            return Err(storage());
        }
        if state.in_flight {
            apply_failure(&mut state, unknown, now);
            transaction.execute("UPDATE network_request_attempts SET outcome='unknown', finished_ms=?3, failure_json=?4 WHERE operation_hash=?1 AND sequence=?2",
                params![operation, sql_int(state.sequence)?, sql_int(now)?, serde_json::to_string(&unknown).map_err(|_| storage())?]).map_err(|_| storage())?;
        }
        let permit = if state.stopped {
            QueuePermit::Stopped(state.failure)
        } else if now < state.ready_at {
            QueuePermit::Waiting(Duration::from_millis(state.ready_at - now), state.failure)
        } else {
            state.sequence = u64::try_from(transaction.query_row("SELECT COALESCE(MAX(sequence), 0)+1 FROM network_request_attempts WHERE operation_hash=?1", [&operation], |row| row.get::<_, i64>(0)).map_err(|_| storage())?).map_err(|_| storage())?;
            state.in_flight = true;
            transaction.execute("INSERT INTO network_request_attempts(operation_hash, sequence, started_ms, outcome) VALUES (?1,?2,?3,'in_flight')",
                    params![operation, sql_int(state.sequence)?, sql_int(now)?]).map_err(|_| storage())?;
            QueuePermit::Ready(state.sequence)
        };
        transaction.execute("INSERT INTO network_queue_state VALUES(?1,?2) ON CONFLICT(operation_hash) DO UPDATE SET state_json=excluded.state_json",
            params![operation, serde_json::to_string(&state).map_err(|_| storage())?]).map_err(|_| storage())?;
        transaction.commit().map_err(|_| storage())?;
        Ok(permit)
    }

    /// Retains the outcome before the queue is allowed to reschedule.
    /// Call success only after the response has passed protocol validation.
    /// # Errors
    /// Rejects a foreign attempt and failed persistence.
    pub fn finish(
        &self,
        operation: &[u8],
        sequence: u64,
        failure: Option<NetworkFailure>,
        now: u64,
    ) -> Result<(), NetworkFailure> {
        let now = self.monotonic_now(now)?;
        let operation = key(operation);
        let mut connection = self.0.lock().map_err(|_| storage())?;
        let transaction = connection.transaction().map_err(|_| storage())?;
        let encoded: String = transaction
            .query_row(
                "SELECT state_json FROM network_queue_state WHERE operation_hash=?1",
                [&operation],
                |row| row.get(0),
            )
            .map_err(|_| storage())?;
        let mut state: State = serde_json::from_str(&encoded).map_err(|_| storage())?;
        if !state.in_flight || state.sequence != sequence {
            return Err(storage());
        }
        let failure_json = failure
            .map(|value| serde_json::to_string(&value).map_err(|_| storage()))
            .transpose()?;
        transaction.execute("UPDATE network_request_attempts SET outcome=?3, failure_json=?4, finished_ms=?5 WHERE operation_hash=?1 AND sequence=?2",
            params![operation, sql_int(sequence)?, if failure.is_some() { "failed" } else { "completed" }, failure_json, sql_int(now)?]).map_err(|_| storage())?;
        if let Some(failure) = failure {
            apply_failure(&mut state, failure, now);
            transaction
                .execute(
                    "UPDATE network_queue_state SET state_json=?2 WHERE operation_hash=?1",
                    params![
                        operation,
                        serde_json::to_string(&state).map_err(|_| storage())?
                    ],
                )
                .map_err(|_| storage())?;
        } else {
            transaction
                .execute(
                    "DELETE FROM network_queue_state WHERE operation_hash=?1",
                    [&operation],
                )
                .map_err(|_| storage())?;
            transaction
                .execute(
                    "DELETE FROM network_envelope_cursors WHERE operation_hash=?1",
                    [&operation],
                )
                .map_err(|_| storage())?;
        }
        transaction.commit().map_err(|_| storage())
    }

    /// Indicates that cost/usage is a lower bound after an earlier unknown attempt.
    /// # Errors
    /// Returns a storage failure rather than guessing a zero cost.
    pub fn has_unknown_usage(&self, operation: &[u8]) -> Result<bool, NetworkFailure> {
        self.0.lock().map_err(|_| storage())?.query_row(
            "SELECT EXISTS(SELECT 1 FROM network_request_attempts WHERE operation_hash=?1 AND (outcome IN ('in_flight','unknown') OR (outcome='failed' AND json_extract(failure_json,'$.acceptance') != 'not_sent')))",
            [key(operation)], |row| row.get(0)).map_err(|_| storage())
    }
}

fn apply_failure(state: &mut State, failure: NetworkFailure, now: u64) {
    state.in_flight = false;
    state.failure = failure;
    let connection = failure.acceptance == Acceptance::NotSent
        && matches!(
            failure.kind,
            ErrorKind::ConnectionUnavailable | ErrorKind::Timeout
        );
    if connection {
        state.connections = state.connections.saturating_add(1);
    } else {
        state.connections = 0;
    }
    match crate::decide(
        failure,
        state.replay,
        state.attempt,
        state.connections,
        state.max_attempts,
        0,
    ) {
        RetryDecision::RetryAfter(delay) | RetryDecision::DeferredUntil(delay) => {
            state.ready_at = now.saturating_add(crate::duration_millis(delay));
            if !connection {
                state.attempt = state.attempt.saturating_add(1);
            }
        }
        RetryDecision::Stop | RetryDecision::Reconcile => state.stopped = true,
    }
}

pub fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, crate::duration_millis)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exact_control_exchange_survives_more_than_four_transient_failures_and_restart() {
        let path = std::env::temp_dir().join(format!(
            "network-control-recovery-{}-{}.sqlite3",
            std::process::id(),
            now_millis()
        ));
        let failure = NetworkFailure::new(
            ErrorKind::Timeout,
            Acceptance::Unknown,
            Phase::ResponseHeaders,
        );
        let mut now = 1_000;
        for attempt in 1..=6 {
            let journal = RequestJournal::open(&path).unwrap();
            let QueuePermit::Ready(sequence) = journal
                .prepare(b"same-control-frame", Replay::ReplayExact, 4, now)
                .unwrap()
            else {
                panic!("valid authority must retain and retry control attempt {attempt}");
            };
            assert_eq!(sequence, attempt);
            journal
                .finish(b"same-control-frame", sequence, Some(failure), now)
                .unwrap();
            let QueuePermit::Waiting(delay, _) = journal
                .prepare(b"same-control-frame", Replay::ReplayExact, 4, now)
                .unwrap()
            else {
                panic!("temporary outage must not terminate the Worker");
            };
            assert!(delay <= Duration::from_mins(1));
            now += 61_000;
        }
        let journal = RequestJournal::open(&path).unwrap();
        let QueuePermit::Ready(sequence) = journal
            .prepare(b"same-control-frame", Replay::ReplayExact, 4, now)
            .unwrap()
        else {
            panic!("recovery");
        };
        journal
            .finish(b"same-control-frame", sequence, None, now)
            .unwrap();
        assert_eq!(sequence, 7);
        drop(journal);
        let _ = std::fs::remove_file(path);
    }
    #[test]
    fn cursor_binding_and_clock_rollback_preserve_the_logical_request() {
        let path = std::env::temp_dir().join(format!(
            "network-cursor-{}-{}.sqlite3",
            std::process::id(),
            now_millis()
        ));
        let journal = RequestJournal::open(&path).unwrap();
        let cursor = EnvelopeCursor {
            last_sequence: 3,
            ack_sequence: 7,
            ..Default::default()
        };
        journal.bind_envelope(b"batch", &cursor).unwrap();
        let QueuePermit::Ready(sequence) = journal
            .prepare(b"batch", Replay::ReplayExact, 4, 100_000)
            .unwrap()
        else {
            panic!("ready")
        };
        journal
            .finish(
                b"batch",
                sequence,
                Some(NetworkFailure::new(
                    ErrorKind::Timeout,
                    Acceptance::Unknown,
                    Phase::ResponseBody,
                )),
                100_000,
            )
            .unwrap();
        drop(journal);
        let journal = RequestJournal::open(&path).unwrap();
        assert_eq!(
            journal
                .bind_envelope(
                    b"batch",
                    &EnvelopeCursor {
                        last_sequence: 9,
                        ack_sequence: 8,
                        ..Default::default()
                    }
                )
                .unwrap(),
            cursor
        );
        let QueuePermit::Waiting(delay, _) = journal
            .prepare(b"batch", Replay::ReplayExact, 4, 1)
            .unwrap()
        else {
            panic!("wait")
        };
        assert!(delay <= Duration::from_secs(5));
        assert!(matches!(
            journal
                .prepare(b"batch", Replay::ReplayExact, 4, 106_000)
                .unwrap(),
            QueuePermit::Ready(_)
        ));
        drop(journal);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn completed_inference_cannot_be_resent_after_restart() {
        let path = std::env::temp_dir().join(format!(
            "network-completed-{}-{}.sqlite3",
            std::process::id(),
            now_millis()
        ));
        let journal = RequestJournal::open(&path).unwrap();
        let QueuePermit::Ready(sequence) = journal
            .prepare(b"inference", Replay::RetryInference, 4, 100)
            .unwrap()
        else {
            panic!("ready")
        };
        journal.finish(b"inference", sequence, None, 101).unwrap();
        drop(journal);
        let reopened = RequestJournal::open(&path).unwrap();
        assert!(matches!(
            reopened
                .prepare(b"inference", Replay::RetryInference, 4, 102)
                .unwrap(),
            QueuePermit::Stopped(NetworkFailure {
                phase: Phase::Persist,
                ..
            })
        ));
        drop(reopened);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn restart_preserves_budget_and_unknown_acceptance() {
        let path = std::env::temp_dir().join(format!(
            "network-journal-{}-{}.sqlite3",
            std::process::id(),
            now_millis()
        ));
        let failure =
            NetworkFailure::new(ErrorKind::Timeout, Acceptance::Unknown, Phase::ResponseBody);
        let key = b"stable request bytes";
        let mut now = 1000;
        for attempt in 1..=4 {
            let journal = RequestJournal::open(&path).unwrap();
            let QueuePermit::Ready(sequence) = journal
                .prepare(key, Replay::RetryInference, 4, now)
                .unwrap()
            else {
                panic!("expected attempt {attempt}")
            };
            journal.finish(key, sequence, Some(failure), now).unwrap();
            assert!(journal.has_unknown_usage(key).unwrap());
            now += 60_000;
        }
        let journal = RequestJournal::open(&path).unwrap();
        assert!(matches!(
            journal
                .prepare(key, Replay::RetryInference, 4, now)
                .unwrap(),
            QueuePermit::Stopped(_)
        ));
        // A different request gets its own budget. Its interrupted unsafe write requires reconciliation.
        assert!(matches!(
            journal
                .prepare(b"mutation", Replay::ReconcileFirst, 4, now)
                .unwrap(),
            QueuePermit::Ready(_)
        ));
        drop(journal);
        let journal = RequestJournal::open(&path).unwrap();
        assert!(matches!(
            journal
                .prepare(b"mutation", Replay::ReconcileFirst, 4, now + 1)
                .unwrap(),
            QueuePermit::Stopped(_)
        ));
        drop(journal);
        let _ = std::fs::remove_file(path);
    }
}
