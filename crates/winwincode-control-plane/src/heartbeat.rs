// SPDX-License-Identifier: Apache-2.0
//!
//! `WinWinCode` heartbeat: liveness + phase progress for long product chains.
//!
//! Different from per-request HTTP deadline:
//! - HTTP timeout = one model call must return by T
//! - Heartbeat = the *job* is alive and advancing through phases
//!
//! A silent process with an open socket still beats; a wedged process stops
//! beating and is detected as `STALLED`.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Heartbeat {
    pub job_id: String,
    pub phase: String,
    pub detail: String,
    pub beat_count: u64,
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
    path: PathBuf,
    started: SystemTime,
    beats: AtomicU64,
    last_unix_ms: AtomicU64,
    running: Arc<AtomicBool>,
    stall_after_ms: u64,
}

/// `Duration` → whole milliseconds, saturating instead of truncating `u128`.
fn duration_ms(value: Duration) -> u64 {
    u64::try_from(value.as_millis()).unwrap_or(u64::MAX)
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, duration_ms)
}

impl HeartbeatHandle {
    /// Start a heartbeat file under `dir` (usually the task workdir).
    #[must_use]
    pub fn start(dir: &Path, job_id: impl Into<String>, stall_after_ms: u64) -> Arc<Self> {
        let _ = fs::create_dir_all(dir);
        let handle = Arc::new(Self {
            job_id: job_id.into(),
            path: dir.join("heartbeat.json"),
            started: SystemTime::now(),
            beats: AtomicU64::new(0),
            last_unix_ms: AtomicU64::new(now_unix_ms()),
            running: Arc::new(AtomicBool::new(true)),
            stall_after_ms: stall_after_ms.max(1_000),
        });
        let bg = Arc::clone(&handle);
        std::thread::spawn(move || {
            while bg.running.load(Ordering::SeqCst) {
                bg.beat("idle", "background tick");
                std::thread::sleep(Duration::from_secs(5));
            }
        });
        handle.beat("start", "heartbeat started");
        handle
    }

    pub fn beat(&self, phase: &str, detail: &str) {
        let count = self.beats.fetch_add(1, Ordering::SeqCst) + 1;
        let unix_ms = now_unix_ms();
        self.last_unix_ms.store(unix_ms, Ordering::SeqCst);
        let wall = self.started.elapsed().map_or(0, duration_ms);
        let record = Heartbeat {
            job_id: self.job_id.clone(),
            phase: phase.to_owned(),
            detail: detail.to_owned(),
            beat_count: count,
            unix_ms,
            wall_clock_ms: wall,
            status: HeartbeatStatus::Alive,
        };
        write_heartbeat(&self.path, &record);
    }

    /// True when no beat within stall window (job wedged).
    #[must_use]
    pub fn is_stalled(&self) -> bool {
        let last = self.last_unix_ms.load(Ordering::SeqCst);
        now_unix_ms().saturating_sub(last) > self.stall_after_ms
    }

    pub fn finish(&self, ok: bool, detail: &str) {
        let count = self.beats.fetch_add(1, Ordering::SeqCst) + 1;
        let unix_ms = now_unix_ms();
        self.last_unix_ms.store(unix_ms, Ordering::SeqCst);
        let wall = self.started.elapsed().map_or(0, duration_ms);
        write_heartbeat(
            &self.path,
            &Heartbeat {
                job_id: self.job_id.clone(),
                phase: "finished".to_owned(),
                detail: detail.to_owned(),
                beat_count: count,
                unix_ms,
                wall_clock_ms: wall,
                status: if ok {
                    HeartbeatStatus::Finished
                } else {
                    HeartbeatStatus::Failed
                },
            },
        );
        self.running.store(false, Ordering::SeqCst);
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

fn write_heartbeat(path: &Path, record: &Heartbeat) {
    if let Ok(text) = serde_json::to_string_pretty(record) {
        let _ = fs::write(path, format!("{text}\n"));
    }
}

/// Read last heartbeat; `None` if missing/corrupt.
#[must_use]
pub fn read_heartbeat(path: &Path) -> Option<Heartbeat> {
    let text = fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// Supervisor check: classify a heartbeat file as alive/stalled.
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
    use std::time::Duration;

    #[test]
    fn heartbeat_writes_and_classifies() {
        let dir = std::env::temp_dir().join(format!("wwc-hb-{}", now_unix_ms()));
        let handle = HeartbeatHandle::start(&dir, "job-test", 60_000);
        handle.beat("compose", "waiting seats");
        assert!(!handle.is_stalled());
        let hb = read_heartbeat(handle.path()).expect("hb");
        assert_eq!(hb.phase, "compose");
        assert!(hb.beat_count >= 2);
        assert_eq!(classify_heartbeat(handle.path(), 60_000), HeartbeatStatus::Alive);
        handle.finish(true, "done");
        assert_eq!(classify_heartbeat(handle.path(), 60_000), HeartbeatStatus::Finished);
        std::thread::sleep(Duration::from_millis(10));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn stalled_when_no_beat() {
        let dir = std::env::temp_dir().join(format!("wwc-hb-stall-{}", now_unix_ms()));
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("heartbeat.json");
        write_heartbeat(
            &path,
            &Heartbeat {
                job_id: "old".to_owned(),
                phase: "seat".to_owned(),
                detail: "wedged".to_owned(),
                beat_count: 1,
                unix_ms: now_unix_ms().saturating_sub(10_000),
                wall_clock_ms: 10,
                status: HeartbeatStatus::Alive,
            },
        );
        assert_eq!(classify_heartbeat(&path, 1_000), HeartbeatStatus::Stalled);
        let _ = fs::remove_dir_all(&dir);
    }
}
