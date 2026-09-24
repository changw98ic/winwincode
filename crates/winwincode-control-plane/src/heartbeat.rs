// SPDX-License-Identifier: Apache-2.0
//!
//! `WinWinCode` heartbeat: liveness and phase progress for long product chains.
//!
//! Different from per-request HTTP deadline:
//! - HTTP timeout = one model call must return by T
//! - Heartbeat = the job is alive and advancing through phases
//!
//! A silent process with an open socket still beats; a wedged process stops
//! beating and is detected as Stalled.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError, Weak};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Heartbeat {
    pub job_id: String,
    pub phase: String,
    pub detail: String,
    pub beat_count: u64,
    pub attempt_generation: u64,
    pub unix_ms: u64,
    pub wall_clock_ms: u64,
    pub status: HeartbeatStatus,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum HeartbeatStatus {
    Alive,
    Stalled,
    Finished,
    Failed,
}

pub struct HeartbeatHandle {
    job_id: String,
    attempt_generation: u64,
    path: PathBuf,
    started: SystemTime,
    stall_after_ms: u64,
    writer: Mutex<WriterState>,
    writer_changed: Condvar,
}

struct WriterState {
    running: bool,
    next_sequence: u64,
    background: Option<JoinHandle<()>>,
}

/// Duration -> whole milliseconds, saturating instead of truncating u128.
fn duration_ms(value: Duration) -> u64 {
    u64::try_from(value.as_millis()).unwrap_or(u64::MAX)
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, duration_ms)
}

impl HeartbeatHandle {
    /// Starts a heartbeat file under dir (usually the task workdir).
    #[must_use]
    pub fn start(dir: &Path, job_id: impl Into<String>, stall_after_ms: u64) -> Arc<Self> {
        Self::start_with_generation(dir, job_id, 1, stall_after_ms)
    }

    /// Starts one run attempt and binds every record to its generation.
    #[must_use]
    pub fn start_with_generation(
        dir: &Path,
        job_id: impl Into<String>,
        attempt_generation: u64,
        stall_after_ms: u64,
    ) -> Arc<Self> {
        let _ = fs::create_dir_all(dir);
        let handle = Arc::new(Self {
            job_id: job_id.into(),
            attempt_generation,
            path: dir.join("heartbeat.json"),
            started: SystemTime::now(),
            stall_after_ms: stall_after_ms.max(1_000),
            writer: Mutex::new(WriterState {
                running: true,
                next_sequence: 1,
                background: None,
            }),
            writer_changed: Condvar::new(),
        });
        let background = spawn_background(Arc::downgrade(&handle));
        handle
            .writer
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .background = Some(background);
        let _ = handle.beat("start", "heartbeat started");
        handle
    }

    /// Publishes one alive record for the current attempt generation.
    ///
    /// Returns `false` when the writer is terminal or the persistent monotonic
    /// guard rejects a stale record.
    ///
    /// # Errors
    ///
    /// Returns an error when the heartbeat cannot be serialized or persisted.
    pub fn beat(&self, phase: &str, detail: &str) -> io::Result<bool> {
        let mut writer = self.writer.lock().unwrap_or_else(PoisonError::into_inner);
        if !writer.running {
            return Ok(false);
        }
        let record = self.record_locked(&mut writer, phase, detail, HeartbeatStatus::Alive);
        write_heartbeat(&self.path, &record)
    }

    /// True when no beat exists or none arrived within the stall window.
    #[must_use]
    pub fn is_stalled(&self) -> bool {
        read_heartbeat(&self.path).is_none_or(|heartbeat| {
            now_unix_ms().saturating_sub(heartbeat.unix_ms) > self.stall_after_ms
        })
    }

    /// Publishes one absorbing terminal state and drains the background writer.
    ///
    /// # Errors
    ///
    /// Returns an error when terminal publication loses to a newer/terminal
    /// record or the heartbeat cannot be persisted.
    pub fn finish(&self, ok: bool, detail: &str) -> io::Result<()> {
        let background = {
            let mut writer = self.writer.lock().unwrap_or_else(PoisonError::into_inner);
            if !writer.running {
                return Ok(());
            }
            let record = self.record_locked(
                &mut writer,
                "finished",
                detail,
                if ok {
                    HeartbeatStatus::Finished
                } else {
                    HeartbeatStatus::Failed
                },
            );
            if !write_heartbeat(&self.path, &record)? {
                return Err(io::Error::other(
                    "terminal heartbeat lost to a newer or terminal record",
                ));
            }
            writer.running = false;
            self.writer_changed.notify_all();
            writer.background.take()
        };
        if let Some(background) = background {
            let _ = background.join();
        }
        Ok(())
    }

    fn record_locked(
        &self,
        writer: &mut WriterState,
        phase: &str,
        detail: &str,
        status: HeartbeatStatus,
    ) -> Heartbeat {
        let beat_count = writer.next_sequence;
        writer.next_sequence = writer.next_sequence.saturating_add(1);
        Heartbeat {
            job_id: self.job_id.clone(),
            phase: phase.to_owned(),
            detail: detail.to_owned(),
            beat_count,
            attempt_generation: self.attempt_generation,
            unix_ms: now_unix_ms(),
            wall_clock_ms: self.started.elapsed().map_or(0, duration_ms),
            status,
        }
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

fn spawn_background(handle: Weak<HeartbeatHandle>) -> JoinHandle<()> {
    thread::spawn(move || {
        loop {
            let Some(handle) = handle.upgrade() else {
                return;
            };
            let writer = handle.writer.lock().unwrap_or_else(PoisonError::into_inner);
            if !writer.running {
                return;
            }
            let (writer, timeout) = handle
                .writer_changed
                .wait_timeout(writer, Duration::from_secs(5))
                .unwrap_or_else(PoisonError::into_inner);
            if timeout.timed_out() && writer.running {
                drop(writer);
                let _ = handle.beat("idle", "background tick");
            } else if !writer.running {
                return;
            }
        }
    })
}

/// Atomically replaces one complete record while rejecting stale writers.
///
/// The caller owns the in-process publication boundary. Persistent checks make
/// late staging renames and old-attempt records fail closed as well.
fn write_heartbeat(path: &Path, record: &Heartbeat) -> io::Result<bool> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    if let Some(current) = read_heartbeat(path)
        && (current.job_id != record.job_id
            || current.attempt_generation != record.attempt_generation
            || record.beat_count <= current.beat_count
            || matches!(
                current.status,
                HeartbeatStatus::Finished | HeartbeatStatus::Failed
            ))
    {
        return Ok(false);
    }
    let text = serde_json::to_string_pretty(record)
        .map_err(|error| io::Error::other(error.to_string()))?;
    let sequence = STAGING_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let staging = path.with_file_name(format!(".{}.{}.tmp", std::process::id(), sequence));
    fs::write(&staging, format!("{text}\n"))?;
    fs::rename(&staging, path)?;
    Ok(true)
}

static STAGING_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Reads the last heartbeat; None if missing or corrupt.
#[must_use]
pub fn read_heartbeat(path: &Path) -> Option<Heartbeat> {
    let text = fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// Classifies a heartbeat file as alive or stalled; terminal states absorb.
#[must_use]
pub fn classify_heartbeat(path: &Path, stall_after_ms: u64) -> HeartbeatStatus {
    match read_heartbeat(path) {
        None => HeartbeatStatus::Stalled,
        Some(hb) => match hb.status {
            HeartbeatStatus::Finished | HeartbeatStatus::Failed => hb.status,
            HeartbeatStatus::Alive | HeartbeatStatus::Stalled => {
                if now_unix_ms().saturating_sub(hb.unix_ms) > stall_after_ms {
                    HeartbeatStatus::Stalled
                } else {
                    HeartbeatStatus::Alive
                }
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn directory(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "wwc-{name}-{}-{}",
            std::process::id(),
            now_unix_ms()
        ))
    }

    fn record(
        job_id: &str,
        attempt_generation: u64,
        beat_count: u64,
        status: HeartbeatStatus,
    ) -> Heartbeat {
        Heartbeat {
            job_id: job_id.to_owned(),
            phase: if matches!(status, HeartbeatStatus::Finished | HeartbeatStatus::Failed) {
                "finished"
            } else {
                "alive"
            }
            .to_owned(),
            detail: "test record".to_owned(),
            beat_count,
            attempt_generation,
            unix_ms: now_unix_ms(),
            wall_clock_ms: 1,
            status,
        }
    }

    #[test]
    fn heartbeat_writes_and_classifies() {
        let dir = directory("heartbeat");
        let handle = HeartbeatHandle::start(&dir, "job-test", 60_000);
        handle.beat("compose", "waiting seats").expect("beat");
        assert!(!handle.is_stalled());
        let hb = read_heartbeat(handle.path()).expect("heartbeat");
        assert_eq!(hb.phase, "compose");
        assert!(hb.beat_count >= 2);
        assert_eq!(
            classify_heartbeat(handle.path(), 60_000),
            HeartbeatStatus::Alive
        );
        handle.finish(true, "done").expect("finish");
        assert_eq!(
            classify_heartbeat(handle.path(), 60_000),
            HeartbeatStatus::Finished
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn stalled_when_no_beat() {
        let dir = directory("heartbeat-stall");
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("heartbeat.json");
        let mut old = record("old", 1, 1, HeartbeatStatus::Alive);
        old.unix_ms = now_unix_ms().saturating_sub(10_000);
        write_heartbeat(&path, &old).expect("stalled heartbeat write");
        assert_eq!(classify_heartbeat(&path, 1_000), HeartbeatStatus::Stalled);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn terminal_state_rejects_late_and_out_of_order_writes() {
        let dir = directory("heartbeat-terminal");
        let path = dir.join("heartbeat.json");

        assert!(
            write_heartbeat(&path, &record("run", 2, 3, HeartbeatStatus::Alive))
                .expect("current write")
        );
        assert!(
            !write_heartbeat(&path, &record("run", 2, 2, HeartbeatStatus::Alive))
                .expect("stale sequence")
        );
        assert!(
            !write_heartbeat(&path, &record("run", 1, 99, HeartbeatStatus::Alive))
                .expect("old attempt")
        );
        assert!(
            write_heartbeat(&path, &record("run", 2, 4, HeartbeatStatus::Finished))
                .expect("terminal write")
        );
        assert!(
            !write_heartbeat(&path, &record("run", 2, 5, HeartbeatStatus::Alive))
                .expect("late alive write")
        );
        assert_eq!(
            read_heartbeat(&path).expect("terminal heartbeat").status,
            HeartbeatStatus::Finished
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn background_record_created_before_finish_cannot_rename_after_terminal() {
        let dir = directory("heartbeat-race");
        let handle = HeartbeatHandle::start_with_generation(&dir, "run", 7, 60_000);
        let background_record = {
            let mut writer = handle.writer.lock().expect("writer lock");
            handle.record_locked(
                &mut writer,
                "idle",
                "background tick",
                HeartbeatStatus::Alive,
            )
        };

        handle
            .finish(true, "finished while background write pending")
            .expect("finish");
        assert!(
            !write_heartbeat(handle.path(), &background_record)
                .expect("late background staging rename")
        );
        assert_eq!(
            classify_heartbeat(handle.path(), 60_000),
            HeartbeatStatus::Finished
        );
        assert!(
            !handle
                .beat("late", "old phase")
                .expect("post-terminal beat")
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_heartbeat_is_stalled() {
        let dir = directory("heartbeat-missing");
        let handle = HeartbeatHandle::start(&dir, "run", 60_000);
        fs::remove_file(handle.path()).expect("remove heartbeat");
        assert!(handle.is_stalled());
        let _ = fs::remove_dir_all(&dir);
    }
}
