// SPDX-License-Identifier: Apache-2.0

//! WORKER-100.5 coverage: the bounded, redacted worker-subprocess logs —
//! explicit caps and rotation, ingest-time filtering of credentials,
//! absolute paths, and model bodies, idempotent terminal exit facts, and
//! the safe diagnostics surface that correlates an abnormal exit to its
//! unique `WorkerSession`.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use winwincode_device_client::supervisor::{WORKER_STATE_CRASHED, WORKER_STATE_EXITED};
use winwincode_device_client::worker_logs::{
    WorkerExitRecordOutcome, WorkerLogConfig, WorkerLogContentKind, WorkerLogError,
    WorkerLogRecorder, WorkerLogStream,
};

static NEXT_TEMP_ROOT: AtomicU64 = AtomicU64::new(1);

fn temporary_root(name: &str) -> PathBuf {
    let suffix = NEXT_TEMP_ROOT.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "winwincode-device-worker-logs-{name}-{}-{suffix}",
        std::process::id()
    ))
}

fn cleanup(root: &Path) {
    let _ = fs::remove_dir_all(root);
}

/// Small bounds so rotation and the model-body backstop are reachable with
/// a handful of appends.
fn tight_config() -> WorkerLogConfig {
    WorkerLogConfig {
        max_segment_bytes: 128,
        max_segments_per_stream: 3,
        max_diagnostic_line_bytes: 64,
        retention_window: Duration::from_hours(24 * 10),
        max_retained_sessions: 8,
    }
}

const SESSION: &str = "wks_TESTWORKERSESSION0000001";
const INSTANCE: &str = "winst_TESTINSTANCE00000001";
const STAMP: &str = "2026-09-04T00:00:00.000Z";

fn recorder(root: &Path, config: WorkerLogConfig) -> WorkerLogRecorder {
    WorkerLogRecorder::open(root, config).expect("worker log recorder opens")
}

/// Every byte stored under one session: meta plus all log segments.
fn stored_bytes(root: &Path, session: &str) -> String {
    let directory = root.join(session);
    let mut stored = String::new();
    let entries = fs::read_dir(&directory).expect("session directory exists");
    for entry in entries {
        let entry = entry.expect("session directory entry");
        let text = fs::read_to_string(entry.path()).expect("stored file is valid UTF-8");
        stored.push_str(&text);
    }
    stored
}

fn segment_names(root: &Path, session: &str, stem: &str) -> Vec<String> {
    let directory = root.join(session);
    let mut names = Vec::new();
    let entries = fs::read_dir(&directory).expect("session directory exists");
    for entry in entries {
        let entry = entry.expect("session directory entry");
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if name == format!("{stem}.log") || name.starts_with(&format!("{stem}.")) {
            names.push(name.to_owned());
        }
    }
    names.sort();
    names
}

fn append_line(recorder: &WorkerLogRecorder, session: &str, stream: WorkerLogStream, line: &str) {
    recorder
        .append(
            session,
            INSTANCE,
            stream,
            WorkerLogContentKind::Diagnostic,
            format!("{line}\n").as_bytes(),
            STAMP,
        )
        .expect("diagnostic append");
}

#[test]
fn credentials_never_reach_storage_or_diagnostics() {
    let root = temporary_root("credentials");
    let logs = recorder(&root, tight_config());
    let secrets = [
        "ghp_SHORTSECRET0001",
        "Bearer SESSIONTOKEN0009",
        "wsc-material-XYZPRIVATE",
        "hunter2",
        "-----BEGIN PRIVATE KEY-----",
        "--token topsecretvalue",
        "abc123abc123abc123abc123abc123abc12",
    ];
    let lines = [
        format!("github token {}", secrets[0]),
        format!("Authorization: {0} {1}", "Bearer", secrets[1]),
        format!("credential file material {}", secrets[2]),
        format!(
            "cloning https://alice:{}@example.invalid/repo.git",
            secrets[3]
        ),
        secrets[4].to_owned(),
        format!("git push --token={} origin", secrets[5]),
        format!("issued material {}", secrets[6]),
    ];
    for line in &lines {
        append_line(&logs, SESSION, WorkerLogStream::Stdout, line);
    }

    let stored = stored_bytes(&root, SESSION);
    for secret in &secrets {
        assert!(
            !stored.contains(secret),
            "secret leaked to storage: {secret}"
        );
    }
    assert!(stored.contains("[REDACTED credential]"));

    let summary = logs
        .summary(SESSION)
        .expect("summary reads")
        .expect("session exists");
    assert_eq!(
        summary.lines_redacted_credential,
        u64::try_from(lines.len()).expect("counter fits")
    );
    let encoded = serde_json::to_string(&summary).expect("summary serializes");
    for secret in &secrets {
        assert!(!encoded.contains(secret), "secret leaked to diagnostics");
    }
    cleanup(&root);
}

#[test]
fn absolute_paths_never_reach_storage_or_diagnostics() {
    let root = temporary_root("paths");
    let logs = recorder(&root, tight_config());
    let masked_paths = [
        "/Users/alice/secret-project/token.txt",
        "/etc/passwd",
        "C:\\Users\\bob\\secret.txt",
        "\\\\fileserver\\share\\secret.pem",
        "file:///home/carol/secret.txt",
        "~/private/secret-notes.md",
        "/usr/local/sbin",
        "/usr/bin",
    ];
    append_line(
        &logs,
        SESSION,
        WorkerLogStream::Stdout,
        &format!("failed to read {}", masked_paths[0]),
    );
    append_line(
        &logs,
        SESSION,
        WorkerLogStream::Stderr,
        &format!("checking {} and {}", masked_paths[1], masked_paths[6]),
    );
    append_line(
        &logs,
        SESSION,
        WorkerLogStream::Stdout,
        &format!("win {} unc {}", masked_paths[2], masked_paths[3]),
    );
    append_line(
        &logs,
        SESSION,
        WorkerLogStream::Stdout,
        &format!("{} {}", masked_paths[4], masked_paths[5]),
    );
    append_line(
        &logs,
        SESSION,
        WorkerLogStream::Stderr,
        &format!("PATH={}:{}", masked_paths[6], masked_paths[7]),
    );
    // Relative paths survive: masking is selective, not a whole-line refusal.
    append_line(
        &logs,
        SESSION,
        WorkerLogStream::Stdout,
        "compiled src/main.rs fine",
    );

    let stored = stored_bytes(&root, SESSION);
    for path in masked_paths {
        assert!(!stored.contains(path), "absolute path leaked: {path}");
    }
    assert!(!stored.contains(&root.to_string_lossy().to_string()));
    assert!(stored.contains("[PATH]"));
    assert!(stored.contains("compiled src/main.rs fine"));

    let summary = logs
        .summary(SESSION)
        .expect("summary reads")
        .expect("session exists");
    assert_eq!(summary.absolute_paths_masked, 9);
    let encoded = serde_json::to_string(&summary).expect("summary serializes");
    assert!(!encoded.contains(&root.to_string_lossy().to_string()));
    for path in masked_paths {
        assert!(!encoded.contains(path), "path leaked to diagnostics");
    }
    cleanup(&root);
}

#[test]
fn model_bodies_never_reach_storage_or_diagnostics() {
    let root = temporary_root("model-body");
    let logs = recorder(&root, tight_config());
    let transcript = "assistant message body: the full model answer with details";
    let stats = logs
        .append(
            SESSION,
            INSTANCE,
            WorkerLogStream::Stdout,
            WorkerLogContentKind::ModelContent,
            transcript.as_bytes(),
            STAMP,
        )
        .expect("model content append");
    assert_eq!(
        stats.model_content_bytes_omitted,
        u64::try_from(transcript.len()).expect("counter fits")
    );
    assert_eq!(stats.lines_written, 0);

    // The backstop: an overlong diagnostic line (a transcript arriving on a
    // diagnostic stream) is omitted too.
    let overlong = format!("transcript {}", "x".repeat(200));
    let stats = logs
        .append(
            SESSION,
            INSTANCE,
            WorkerLogStream::Stderr,
            WorkerLogContentKind::Diagnostic,
            format!("{overlong}\n").as_bytes(),
            STAMP,
        )
        .expect("oversize append");
    assert_eq!(stats.lines_omitted_oversize, 1);
    assert_eq!(stats.lines_written, 0);

    append_line(&logs, SESSION, WorkerLogStream::Stdout, "plain diagnostic");

    let stored = stored_bytes(&root, SESSION);
    assert!(!stored.contains(transcript));
    assert!(!stored.contains(&overlong));
    let summary = logs
        .summary(SESSION)
        .expect("summary reads")
        .expect("session exists");
    assert_eq!(
        summary.model_content_bytes_omitted,
        u64::try_from(transcript.len()).expect("counter fits")
    );
    assert_eq!(summary.lines_omitted_oversize, 1);
    let encoded = serde_json::to_string(&summary).expect("summary serializes");
    assert!(!encoded.contains(transcript));
    cleanup(&root);
}

#[test]
fn a_credential_split_across_appends_is_still_redacted() {
    let root = temporary_root("split");
    let logs = recorder(&root, tight_config());
    logs.append(
        SESSION,
        INSTANCE,
        WorkerLogStream::Stdout,
        WorkerLogContentKind::Diagnostic,
        b"token is ghp_",
        STAMP,
    )
    .expect("first fragment");
    logs.append(
        SESSION,
        INSTANCE,
        WorkerLogStream::Stdout,
        WorkerLogContentKind::Diagnostic,
        b"SHORTSECRET\nnext diagnostic line\n",
        STAMP,
    )
    .expect("second fragment");

    let stored = stored_bytes(&root, SESSION);
    assert!(!stored.contains("ghp_SHORTSECRET"));
    assert!(stored.contains("[REDACTED credential]"));
    assert!(stored.contains("next diagnostic line"));
    cleanup(&root);
}

#[test]
fn segments_rotate_within_the_explicit_caps() {
    let root = temporary_root("rotation");
    let logs = recorder(&root, tight_config());
    append_line(
        &logs,
        SESSION,
        WorkerLogStream::Stdout,
        "FIRSTLINE must rotate away",
    );
    for index in 0..30 {
        append_line(
            &logs,
            SESSION,
            WorkerLogStream::Stdout,
            &format!("filler line {index:03} padding padding"),
        );
    }
    append_line(&logs, SESSION, WorkerLogStream::Stdout, "LASTLINE stays");

    let summary = logs
        .summary(SESSION)
        .expect("summary reads")
        .expect("session exists");
    assert!(summary.rotations > 0);
    let segments = segment_names(&root, SESSION, "stdout");
    assert!(
        segments.len() <= tight_config().max_segments_per_stream,
        "too many retained segments: {segments:?}"
    );
    for name in &segments {
        let length = fs::metadata(root.join(SESSION).join(name))
            .expect("segment metadata")
            .len();
        let cap = tight_config().max_segment_bytes
            + u64::try_from(tight_config().max_diagnostic_line_bytes).expect("cap fits");
        assert!(
            length <= cap,
            "segment {name} exceeds the cap ({length} bytes)"
        );
    }
    let retained = segments
        .iter()
        .map(|name| fs::read_to_string(root.join(SESSION).join(name)).expect("segment read"))
        .collect::<String>();
    assert!(
        !retained.contains("FIRSTLINE"),
        "oldest segment was not dropped"
    );
    assert!(retained.contains("LASTLINE"));

    // The stderr stream is bounded independently and untouched here.
    assert!(segment_names(&root, SESSION, "stderr").is_empty());
    cleanup(&root);
}

#[test]
fn abnormal_exit_is_correlated_to_its_unique_worker_session() {
    let root = temporary_root("crash");
    let logs = recorder(&root, WorkerLogConfig::default());
    append_line(
        &logs,
        SESSION,
        WorkerLogStream::Stderr,
        "panic: worker boot failed",
    );
    let outcome = logs
        .record_exit(SESSION, INSTANCE, WORKER_STATE_CRASHED, Some(137), STAMP)
        .expect("exit records");
    assert_eq!(outcome, WorkerExitRecordOutcome::Recorded);

    let reference = logs
        .crash_reference(SESSION)
        .expect("crash reference reads")
        .expect("crashed session has a reference");
    assert_eq!(reference.worker_session_id, SESSION);
    assert_eq!(reference.worker_instance_id, INSTANCE);
    assert_eq!(reference.exit_code, Some(137));
    assert_eq!(reference.recorded_at, STAMP);
    assert_eq!(reference.log_directory, SESSION);
    assert!(root.join(&reference.log_directory).is_dir());

    let summary = logs
        .summary(SESSION)
        .expect("summary reads")
        .expect("session exists");
    let terminal = summary.terminal.expect("terminal fact");
    assert_eq!(terminal.state, WORKER_STATE_CRASHED);
    assert_eq!(terminal.exit_code, Some(137));
    assert_eq!(terminal.worker_instance_id, INSTANCE);
    assert_eq!(summary.worker_session_id, SESSION);
    assert_eq!(summary.stderr_lines, 1);

    let stored = stored_bytes(&root, SESSION);
    assert_eq!(
        stored.matches("[winwincode] worker process exit").count(),
        1,
        "exactly one terminal marker line"
    );
    assert!(stored.contains("state=crashed exitCode=137"));
    cleanup(&root);
}

#[test]
fn repeated_recovery_does_not_duplicate_the_terminal_fact() {
    let root = temporary_root("idempotent");
    let logs = recorder(&root, WorkerLogConfig::default());
    append_line(&logs, SESSION, WorkerLogStream::Stdout, "booting");
    let first = logs
        .record_exit(SESSION, INSTANCE, WORKER_STATE_CRASHED, Some(137), STAMP)
        .expect("first exit record");
    assert_eq!(first, WorkerExitRecordOutcome::Recorded);

    // The same recovery replayed (even with a different observation) is a
    // no-op: no second marker, no meta change.
    let replay = logs
        .record_exit(SESSION, INSTANCE, WORKER_STATE_CRASHED, Some(1), STAMP)
        .expect("replayed exit record");
    assert_eq!(replay, WorkerExitRecordOutcome::AlreadyRecorded);
    let summary = logs
        .summary(SESSION)
        .expect("summary reads")
        .expect("session exists");
    assert_eq!(
        summary.terminal.expect("terminal fact").exit_code,
        Some(137)
    );
    let stored = stored_bytes(&root, SESSION);
    assert_eq!(
        stored.matches("[winwincode] worker process exit").count(),
        1
    );

    // A replacement boot of the same session records its own fresh fact.
    let replacement = "winst_TESTINSTANCE00000002";
    append_line_with_instance(&logs, SESSION, replacement, "replacement boot");
    let second = logs
        .record_exit(
            SESSION,
            replacement,
            WORKER_STATE_EXITED,
            Some(0),
            "2026-09-04T01:00:00.000Z",
        )
        .expect("replacement exit record");
    assert_eq!(second, WorkerExitRecordOutcome::Recorded);
    let summary = logs
        .summary(SESSION)
        .expect("summary reads")
        .expect("session exists");
    let terminal = summary.terminal.expect("terminal fact");
    assert_eq!(terminal.state, WORKER_STATE_EXITED);
    assert_eq!(terminal.worker_instance_id, replacement);
    assert_eq!(terminal.exit_code, Some(0));

    let stored = stored_bytes(&root, SESSION);
    assert_eq!(
        stored.matches("[winwincode] worker process exit").count(),
        2
    );
    assert_eq!(
        stored.matches(&format!("instance={INSTANCE}")).count(),
        1,
        "the first boot's marker stays unique"
    );
    assert!(stored.contains("worker process exit state=crashed exitCode=137"));
    assert!(stored.contains("worker process exit state=exited exitCode=0"));
    cleanup(&root);
}

fn append_line_with_instance(
    recorder: &WorkerLogRecorder,
    session: &str,
    instance: &str,
    line: &str,
) {
    recorder
        .append(
            session,
            instance,
            WorkerLogStream::Stdout,
            WorkerLogContentKind::Diagnostic,
            format!("{line}\n").as_bytes(),
            STAMP,
        )
        .expect("diagnostic append");
}

#[test]
fn exit_facts_survive_a_worker_that_died_without_output() {
    let root = temporary_root("no-output");
    let logs = recorder(&root, WorkerLogConfig::default());
    let outcome = logs
        .record_exit(SESSION, INSTANCE, WORKER_STATE_CRASHED, None, STAMP)
        .expect("exit records");
    assert_eq!(outcome, WorkerExitRecordOutcome::Recorded);

    let summary = logs
        .summary(SESSION)
        .expect("summary reads")
        .expect("exit fact created the session record");
    // The exit fact itself is the retained stdout marker line; the worker
    // wrote nothing.
    assert!(summary.stdout_bytes > 0);
    assert_eq!(summary.stderr_bytes, 0);
    let terminal = summary.terminal.expect("terminal fact");
    assert_eq!(terminal.state, WORKER_STATE_CRASHED);
    assert_eq!(terminal.exit_code, None);
    let reference = logs
        .crash_reference(SESSION)
        .expect("crash reference reads")
        .expect("crash correlation without output");
    assert_eq!(reference.worker_session_id, SESSION);

    // A buffered unterminated fragment is flushed before the marker.
    let root_flush = temporary_root("flush");
    let logs_flush = recorder(&root_flush, WorkerLogConfig::default());
    logs_flush
        .append(
            SESSION,
            INSTANCE,
            WorkerLogStream::Stderr,
            WorkerLogContentKind::Diagnostic,
            b"unterminated tail",
            STAMP,
        )
        .expect("fragment append");
    logs_flush
        .record_exit(SESSION, INSTANCE, WORKER_STATE_EXITED, Some(0), STAMP)
        .expect("exit records");
    let stored = stored_bytes(&root_flush, SESSION);
    assert!(stored.contains("unterminated tail"));
    assert!(stored.contains("worker process exit state=exited"));
    cleanup(&root);
    cleanup(&root_flush);
}

#[test]
fn retention_prunes_expired_and_oldest_beyond_the_cap() {
    let root = temporary_root("prune");
    let config = WorkerLogConfig {
        retention_window: Duration::from_hours(24 * 10),
        max_retained_sessions: 1,
        ..WorkerLogConfig::default()
    };
    let logs = recorder(&root, config);
    for (session, stamp) in [
        (
            "wks_PRUNEOLD0000000000000000001",
            "2026-07-01T00:00:00.000Z",
        ),
        (
            "wks_PRUNEMID0000000000000000001",
            "2026-08-30T00:00:00.000Z",
        ),
        (
            "wks_PRUNENEW0000000000000000001",
            "2026-09-03T00:00:00.000Z",
        ),
    ] {
        logs.append(
            session,
            INSTANCE,
            WorkerLogStream::Stdout,
            WorkerLogContentKind::Diagnostic,
            b"session activity\n",
            stamp,
        )
        .expect("stamped activity");
    }

    let removed = logs.prune("2026-09-04T00:00:00.000Z").expect("prune runs");
    assert_eq!(removed, 2, "the expired session and the cap overflow");
    let sessions = logs.session_ids().expect("session list");
    assert_eq!(sessions, vec!["wks_PRUNENEW0000000000000000001".to_owned()]);

    // Pruning again is idempotent.
    let removed = logs.prune("2026-09-04T00:00:00.000Z").expect("prune runs");
    assert_eq!(removed, 0);
    cleanup(&root);
}

#[cfg(unix)]
#[test]
fn stored_files_are_private() {
    use std::os::unix::fs::PermissionsExt;

    let root = temporary_root("private");
    let logs = recorder(&root, WorkerLogConfig::default());
    append_line(
        &logs,
        SESSION,
        WorkerLogStream::Stdout,
        "private diagnostic",
    );
    logs.record_exit(SESSION, INSTANCE, WORKER_STATE_EXITED, Some(0), STAMP)
        .expect("exit records");

    let mode = |path: &Path| fs::metadata(path).expect("metadata").permissions().mode() & 0o777;
    assert_eq!(mode(&root), 0o700);
    assert_eq!(mode(&root.join(SESSION)), 0o700);
    assert_eq!(mode(&root.join(SESSION).join("stdout.log")), 0o600);
    assert_eq!(mode(&root.join(SESSION).join("meta.json")), 0o600);
    cleanup(&root);
}

#[test]
fn invalid_inputs_are_rejected() {
    let root = temporary_root("invalid");
    let logs = recorder(&root, tight_config());

    let bad_session = logs.append(
        "../escape",
        INSTANCE,
        WorkerLogStream::Stdout,
        WorkerLogContentKind::Diagnostic,
        b"x\n",
        STAMP,
    );
    assert!(matches!(bad_session, Err(WorkerLogError::InvalidInput(_))));

    let bad_stamp = logs.append(
        SESSION,
        INSTANCE,
        WorkerLogStream::Stdout,
        WorkerLogContentKind::Diagnostic,
        b"x\n",
        "not-a-stamp",
    );
    assert!(matches!(bad_stamp, Err(WorkerLogError::InvalidInput(_))));

    let bad_state = logs.record_exit(SESSION, INSTANCE, "running", None, STAMP);
    assert!(matches!(bad_state, Err(WorkerLogError::InvalidInput(_))));

    let bad_instance = logs.record_exit(SESSION, "", WORKER_STATE_EXITED, None, STAMP);
    assert!(matches!(bad_instance, Err(WorkerLogError::InvalidInput(_))));

    let bad_config = WorkerLogConfig {
        max_segment_bytes: 32,
        ..tight_config()
    };
    assert!(matches!(
        WorkerLogRecorder::open(&root.join("bad"), bad_config),
        Err(WorkerLogError::InvalidInput(_))
    ));

    // Unknown sessions answer safe empties.
    assert!(logs.summary(SESSION).expect("summary").is_none());
    assert!(logs.crash_reference(SESSION).expect("reference").is_none());
    cleanup(&root);
}
