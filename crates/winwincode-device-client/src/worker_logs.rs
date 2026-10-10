use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::supervisor::{WORKER_STATE_CRASHED, WORKER_STATE_EXITED, WORKER_STATE_MISSING};

pub const WORKER_LOG_SCHEMA_VERSION: &str = "winwincode.worker-logs.v1";

const STDOUT_STEM: &str = "stdout";
const STDERR_STEM: &str = "stderr";
const META_FILE: &str = "meta.json";
const META_TMP_SUFFIX: &str = ".tmp";
const PATH_MARKER: &str = "[PATH]";
const CREDENTIAL_MARKER: &str = "[REDACTED credential]";
const TERMINAL_MARKER_PREFIX: &str = "[winwincode] worker process exit";

pub const MIN_DIAGNOSTIC_LINE_BYTES: usize = 64;
pub const MAX_DIAGNOSTIC_LINE_BYTES: usize = 1_048_576;
pub const MAX_SEGMENT_BYTES: u64 = 67_108_864;
pub const MAX_SEGMENTS_PER_STREAM: usize = 64;
pub const MAX_RETAINED_SESSIONS: usize = 100_000;
const MAX_SESSION_ID_BYTES: usize = 200;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum WorkerLogStream {
    #[default]
    Stdout,
    Stderr,
}

impl WorkerLogStream {
    #[must_use]
    pub fn file_stem(self) -> &'static str {
        match self {
            Self::Stdout => STDOUT_STEM,
            Self::Stderr => STDERR_STEM,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkerLogContentKind {
    Diagnostic,
    ModelContent,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WorkerLogConfig {
    pub max_segment_bytes: u64,
    pub max_segments_per_stream: usize,
    pub max_diagnostic_line_bytes: usize,
    pub retention_window: Duration,
    pub max_retained_sessions: usize,
}

impl Default for WorkerLogConfig {
    fn default() -> Self {
        Self {
            max_segment_bytes: 64 * 1_024,
            max_segments_per_stream: 4,
            max_diagnostic_line_bytes: 1_024,
            retention_window: Duration::from_hours(24 * 7),
            max_retained_sessions: 64,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WorkerLogAppendStats {
    pub stream: WorkerLogStream,
    pub lines_written: u64,
    pub bytes_written: u64,
    pub lines_omitted_oversize: u64,
    pub lines_redacted_credential: u64,
    pub absolute_paths_masked: u64,
    pub control_bytes_stripped: u64,
    pub model_content_bytes_omitted: u64,
    pub rotations: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkerExitRecordOutcome {
    Recorded,
    AlreadyRecorded,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkerExitFact {
    pub state: String,
    pub exit_code: Option<i64>,
    pub worker_instance_id: String,
    pub recorded_at: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkerLogSummary {
    pub worker_session_id: String,
    pub worker_instance_id: Option<String>,
    pub created_at: String,
    pub last_activity_at: String,
    pub stdout_bytes: u64,
    pub stderr_bytes: u64,
    pub stdout_lines: u64,
    pub stderr_lines: u64,
    pub lines_omitted_oversize: u64,
    pub lines_redacted_credential: u64,
    pub absolute_paths_masked: u64,
    pub control_bytes_stripped: u64,
    pub model_content_bytes_omitted: u64,
    pub rotations: u64,
    pub terminal: Option<WorkerExitFact>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkerLogCrashReference {
    pub worker_session_id: String,
    pub worker_instance_id: String,
    pub exit_code: Option<i64>,
    pub recorded_at: String,
    pub log_directory: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkerLogError {
    InvalidInput(String),
    CorruptMeta { worker_session_id: String },
    Io { context: String },
}

impl WorkerLogError {
    fn invalid(message: impl Into<String>) -> Self {
        Self::InvalidInput(message.into())
    }

    fn io(context: impl Into<String>) -> Self {
        Self::Io {
            context: context.into(),
        }
    }
}

impl fmt::Display for WorkerLogError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidInput(message) => {
                write!(formatter, "worker log input: {message}")
            }
            Self::CorruptMeta { worker_session_id } => {
                write!(
                    formatter,
                    "worker log meta for session {worker_session_id} is corrupt"
                )
            }
            Self::Io { context } => write!(formatter, "worker log storage: {context}"),
        }
    }
}

impl std::error::Error for WorkerLogError {}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SessionMeta {
    schema_version: String,
    worker_session_id: String,
    worker_instance_id: Option<String>,
    created_at: String,
    last_activity_at: String,
    terminal: Option<WorkerExitFact>,
    stdout_lines: u64,
    stderr_lines: u64,
    lines_omitted_oversize: u64,
    lines_redacted_credential: u64,
    absolute_paths_masked: u64,
    control_bytes_stripped: u64,
    model_content_bytes_omitted: u64,
    rotations: u64,
}

impl SessionMeta {
    fn fresh(worker_session_id: &str, worker_instance_id: Option<&str>, stamp: &str) -> Self {
        Self {
            schema_version: WORKER_LOG_SCHEMA_VERSION.to_owned(),
            worker_session_id: worker_session_id.to_owned(),
            worker_instance_id: worker_instance_id.map(str::to_owned),
            created_at: stamp.to_owned(),
            last_activity_at: stamp.to_owned(),
            terminal: None,
            stdout_lines: 0,
            stderr_lines: 0,
            lines_omitted_oversize: 0,
            lines_redacted_credential: 0,
            absolute_paths_masked: 0,
            control_bytes_stripped: 0,
            model_content_bytes_omitted: 0,
            rotations: 0,
        }
    }

    fn touch_stream(&mut self, stream: WorkerLogStream, lines: u64) {
        match stream {
            WorkerLogStream::Stdout => self.stdout_lines = self.stdout_lines.saturating_add(lines),
            WorkerLogStream::Stderr => self.stderr_lines = self.stderr_lines.saturating_add(lines),
        }
    }
}

struct ProcessedLine {
    output: Option<String>,
    credential_redacted: bool,
    paths_masked: u64,
    control_bytes_stripped: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct StreamPending {
    stdout: String,
    stderr: String,
}

impl StreamPending {
    fn is_empty(&self) -> bool {
        self.stdout.is_empty() && self.stderr.is_empty()
    }

    fn stream_mut(&mut self, stream: WorkerLogStream) -> &mut String {
        match stream {
            WorkerLogStream::Stdout => &mut self.stdout,
            WorkerLogStream::Stderr => &mut self.stderr,
        }
    }

    fn stream(&self, stream: WorkerLogStream) -> &str {
        match stream {
            WorkerLogStream::Stdout => &self.stdout,
            WorkerLogStream::Stderr => &self.stderr,
        }
    }
}

struct RecorderState {
    logs_root: PathBuf,
    config: WorkerLogConfig,
    operation_lock: Mutex<()>,
    pending: Mutex<BTreeMap<String, StreamPending>>,
}

#[derive(Clone)]
pub struct WorkerLogRecorder {
    state: Arc<RecorderState>,
}

impl fmt::Debug for WorkerLogRecorder {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WorkerLogRecorder")
            .field("logs_root", &self.state.logs_root)
            .field("config", &self.state.config)
            .finish_non_exhaustive()
    }
}

impl WorkerLogRecorder {
    pub fn open(logs_root: &Path, config: WorkerLogConfig) -> Result<Self, WorkerLogError> {
        validate_config(&config)?;
        create_private_directory(logs_root)?;
        if !logs_root.is_dir() {
            return Err(WorkerLogError::io(
                "logs root is not a directory after creation",
            ));
        }
        Ok(Self {
            state: Arc::new(RecorderState {
                logs_root: logs_root.to_path_buf(),
                config,
                operation_lock: Mutex::new(()),
                pending: Mutex::new(BTreeMap::new()),
            }),
        })
    }

    #[must_use]
    pub fn config(&self) -> WorkerLogConfig {
        self.state.config
    }

    pub fn append(
        &self,
        worker_session_id: &str,
        worker_instance_id: &str,
        stream: WorkerLogStream,
        kind: WorkerLogContentKind,
        chunk: &[u8],
        stamp: &str,
    ) -> Result<WorkerLogAppendStats, WorkerLogError> {
        validate_session_id(worker_session_id)?;
        parse_stamp(stamp)?;
        let _operation_guard = self.lock_operations();
        let mut meta = self.load_or_create_meta(worker_session_id, worker_instance_id, stamp)?;
        stamp.clone_into(&mut meta.last_activity_at);
        if !worker_instance_id.is_empty() {
            meta.worker_instance_id = Some(worker_instance_id.to_owned());
        }

        let mut stats = WorkerLogAppendStats {
            stream,
            ..WorkerLogAppendStats::default()
        };
        match kind {
            WorkerLogContentKind::ModelContent => {
                let omitted = u64::try_from(chunk.len()).unwrap_or(u64::MAX);
                stats.model_content_bytes_omitted = omitted;
                meta.model_content_bytes_omitted =
                    meta.model_content_bytes_omitted.saturating_add(omitted);
            }
            WorkerLogContentKind::Diagnostic => {
                let text = String::from_utf8_lossy(chunk);
                let session_dir = self.session_directory(worker_session_id);
                let stem = stream.file_stem();
                let mut combined = self.take_pending(worker_session_id, stream);
                combined.push_str(&text);

                let mut rest = combined.as_str();
                while let Some((line, remainder)) = rest.split_once('\n') {
                    rest = remainder;
                    let lines = self.process_and_write_line(
                        &session_dir,
                        stem,
                        line,
                        &mut meta,
                        &mut stats,
                    )?;
                    meta.touch_stream(stream, lines);
                }
                let fragment = rest.trim_end_matches('\r');
                if fragment.len() > self.state.config.max_diagnostic_line_bytes {
                    let processed =
                        process_line(fragment, self.state.config.max_diagnostic_line_bytes);
                    apply_line(&mut meta, &mut stats, &processed);
                    self.clear_pending(worker_session_id, stream);
                } else {
                    self.store_pending(worker_session_id, stream, fragment);
                }
            }
        }
        self.persist_meta(worker_session_id, &meta)?;
        Ok(stats)
    }

    pub fn record_exit(
        &self,
        worker_session_id: &str,
        worker_instance_id: &str,
        state: &str,
        exit_code: Option<i64>,
        stamp: &str,
    ) -> Result<WorkerExitRecordOutcome, WorkerLogError> {
        validate_session_id(worker_session_id)?;
        if worker_instance_id.is_empty() {
            return Err(WorkerLogError::invalid(
                "worker instance id must not be empty",
            ));
        }
        if !matches!(
            state,
            WORKER_STATE_EXITED | WORKER_STATE_CRASHED | WORKER_STATE_MISSING
        ) {
            return Err(WorkerLogError::invalid(
                "terminal state must be exited, crashed, or missing",
            ));
        }
        let observed_at = parse_stamp(stamp)?;
        let _operation_guard = self.lock_operations();
        let mut meta = self.load_or_create_meta(worker_session_id, worker_instance_id, stamp)?;
        if meta
            .terminal
            .as_ref()
            .is_some_and(|fact| fact.worker_instance_id == worker_instance_id)
        {
            return Ok(WorkerExitRecordOutcome::AlreadyRecorded);
        }

        let pending = self
            .lock_pending()
            .remove(worker_session_id)
            .unwrap_or_default();
        let session_dir = self.session_directory(worker_session_id);
        for stream in [WorkerLogStream::Stdout, WorkerLogStream::Stderr] {
            let fragment = pending.stream(stream).to_owned();
            if fragment.is_empty() {
                continue;
            }
            let lines = self.process_and_write_line(
                &session_dir,
                stream.file_stem(),
                &fragment,
                &mut meta,
                &mut WorkerLogAppendStats::default(),
            )?;
            meta.touch_stream(stream, lines);
        }

        let code = match exit_code {
            Some(code) => format!("exitCode={code}"),
            None => "exitCode=none".to_owned(),
        };
        let marker = format!(
            "{TERMINAL_MARKER_PREFIX} state={state} {code} \
             session={worker_session_id} instance={worker_instance_id}"
        );
        let rotations =
            write_line_to_stream(&session_dir, STDOUT_STEM, &marker, &self.state.config)?;
        meta.rotations = meta.rotations.saturating_add(rotations);
        meta.stdout_lines = meta.stdout_lines.saturating_add(1);
        meta.terminal = Some(WorkerExitFact {
            state: state.to_owned(),
            exit_code,
            worker_instance_id: worker_instance_id.to_owned(),
            recorded_at: stamp.to_owned(),
        });
        stamp.clone_into(&mut meta.last_activity_at);
        self.persist_meta(worker_session_id, &meta)?;
        self.prune_with(observed_at)?;
        Ok(WorkerExitRecordOutcome::Recorded)
    }

    pub fn summary(
        &self,
        worker_session_id: &str,
    ) -> Result<Option<WorkerLogSummary>, WorkerLogError> {
        let _operation_guard = self.lock_operations();
        let Some(meta) = self.load_meta(worker_session_id)? else {
            return Ok(None);
        };
        let session_dir = self.session_directory(worker_session_id);
        Ok(Some(WorkerLogSummary {
            worker_session_id: meta.worker_session_id,
            worker_instance_id: meta.worker_instance_id,
            created_at: meta.created_at,
            last_activity_at: meta.last_activity_at,
            stdout_bytes: retained_stream_bytes(
                &session_dir,
                STDOUT_STEM,
                self.state.config.max_segments_per_stream,
            ),
            stderr_bytes: retained_stream_bytes(
                &session_dir,
                STDERR_STEM,
                self.state.config.max_segments_per_stream,
            ),
            stdout_lines: meta.stdout_lines,
            stderr_lines: meta.stderr_lines,
            lines_omitted_oversize: meta.lines_omitted_oversize,
            lines_redacted_credential: meta.lines_redacted_credential,
            absolute_paths_masked: meta.absolute_paths_masked,
            control_bytes_stripped: meta.control_bytes_stripped,
            model_content_bytes_omitted: meta.model_content_bytes_omitted,
            rotations: meta.rotations,
            terminal: meta.terminal,
        }))
    }

    pub fn crash_reference(
        &self,
        worker_session_id: &str,
    ) -> Result<Option<WorkerLogCrashReference>, WorkerLogError> {
        let _operation_guard = self.lock_operations();
        let Some(meta) = self.load_meta(worker_session_id)? else {
            return Ok(None);
        };
        let Some(fact) = meta.terminal else {
            return Ok(None);
        };
        if fact.state != WORKER_STATE_CRASHED {
            return Ok(None);
        }
        Ok(Some(WorkerLogCrashReference {
            worker_session_id: meta.worker_session_id,
            worker_instance_id: fact.worker_instance_id,
            exit_code: fact.exit_code,
            recorded_at: fact.recorded_at,
            log_directory: worker_session_id.to_owned(),
        }))
    }

    pub fn session_ids(&self) -> Result<Vec<String>, WorkerLogError> {
        let _operation_guard = self.lock_operations();
        self.session_ids_unlocked()
    }

    fn session_ids_unlocked(&self) -> Result<Vec<String>, WorkerLogError> {
        let mut sessions = Vec::new();
        let entries = fs::read_dir(&self.state.logs_root)
            .map_err(|_| WorkerLogError::io("logs root read"))?;
        for entry in entries {
            let entry = entry.map_err(|_| WorkerLogError::io("logs root entry read"))?;
            if !entry.path().is_dir() {
                continue;
            }
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if validate_session_id(name).is_ok() {
                sessions.push(name.to_owned());
            }
        }
        sessions.sort();
        Ok(sessions)
    }

    pub fn prune(&self, now: &str) -> Result<usize, WorkerLogError> {
        let now = parse_stamp(now)?;
        let _operation_guard = self.lock_operations();
        self.prune_with(now)
    }

    fn prune_with(&self, now: OffsetDateTime) -> Result<usize, WorkerLogError> {
        let window =
            i64::try_from(self.state.config.retention_window.as_secs()).unwrap_or(i64::MAX);
        let mut removed = 0usize;
        let mut retained: Vec<(String, i64)> = Vec::new();
        for session in self.session_ids_unlocked()? {
            match self.load_meta(&session) {
                Ok(Some(meta)) => {
                    let last = if let Ok(stamp) = parse_stamp(&meta.last_activity_at) {
                        stamp.unix_timestamp()
                    } else {
                        i64::MIN
                    };
                    if window > 0 && now.unix_timestamp() - last >= window {
                        remove_session_directory(&self.state.logs_root, &session)?;
                        removed += 1;
                    } else {
                        retained.push((session, last));
                    }
                }
                Ok(None) | Err(WorkerLogError::CorruptMeta { .. }) => {}
                Err(error) => return Err(error),
            }
        }
        while retained.len() > self.state.config.max_retained_sessions {
            let oldest = retained
                .iter()
                .enumerate()
                .min_by_key(|(_, (_, stamp))| *stamp)
                .map(|(index, _)| index);
            let Some(index) = oldest else {
                break;
            };
            let (session, _) = retained.remove(index);
            remove_session_directory(&self.state.logs_root, &session)?;
            removed += 1;
        }
        Ok(removed)
    }

    fn session_directory(&self, worker_session_id: &str) -> PathBuf {
        self.state.logs_root.join(worker_session_id)
    }

    fn lock_pending(&self) -> MutexGuard<'_, BTreeMap<String, StreamPending>> {
        self.state
            .pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn lock_operations(&self) -> MutexGuard<'_, ()> {
        self.state
            .operation_lock
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn take_pending(&self, worker_session_id: &str, stream: WorkerLogStream) -> String {
        let mut pending = self.lock_pending();
        let entry = pending.entry(worker_session_id.to_owned()).or_default();
        std::mem::take(entry.stream_mut(stream))
    }

    fn store_pending(&self, worker_session_id: &str, stream: WorkerLogStream, fragment: &str) {
        let mut pending = self.lock_pending();
        let entry = pending.entry(worker_session_id.to_owned()).or_default();
        fragment.clone_into(entry.stream_mut(stream));
    }

    fn clear_pending(&self, worker_session_id: &str, stream: WorkerLogStream) {
        let mut pending = self.lock_pending();
        if let Some(entry) = pending.get_mut(worker_session_id) {
            entry.stream_mut(stream).clear();
            if entry.is_empty() {
                pending.remove(worker_session_id);
            }
        }
    }

    fn process_and_write_line(
        &self,
        session_dir: &Path,
        stem: &str,
        line: &str,
        meta: &mut SessionMeta,
        stats: &mut WorkerLogAppendStats,
    ) -> Result<u64, WorkerLogError> {
        let processed = process_line(line, self.state.config.max_diagnostic_line_bytes);
        apply_line(meta, stats, &processed);
        let Some(text) = processed.output else {
            return Ok(0);
        };
        let rotations = write_line_to_stream(session_dir, stem, &text, &self.state.config)?;
        meta.rotations = meta.rotations.saturating_add(rotations);
        stats.rotations += rotations;
        stats.lines_written += 1;
        stats.bytes_written = stats
            .bytes_written
            .saturating_add(u64::try_from(text.len()).unwrap_or(u64::MAX))
            .saturating_add(1);
        Ok(1)
    }

    fn load_or_create_meta(
        &self,
        worker_session_id: &str,
        worker_instance_id: &str,
        stamp: &str,
    ) -> Result<SessionMeta, WorkerLogError> {
        if let Some(meta) = self.load_meta(worker_session_id)? {
            return Ok(meta);
        }
        let session_dir = self.session_directory(worker_session_id);
        create_private_directory(&session_dir)?;
        let instance = Some(worker_instance_id).filter(|instance| !instance.is_empty());
        Ok(SessionMeta::fresh(worker_session_id, instance, stamp))
    }

    fn load_meta(&self, worker_session_id: &str) -> Result<Option<SessionMeta>, WorkerLogError> {
        let path = self.session_directory(worker_session_id).join(META_FILE);
        let Ok(text) = fs::read_to_string(&path) else {
            return Ok(None);
        };
        serde_json::from_str(&text)
            .map(Some)
            .map_err(|_| WorkerLogError::CorruptMeta {
                worker_session_id: worker_session_id.to_owned(),
            })
    }

    fn persist_meta(
        &self,
        worker_session_id: &str,
        meta: &SessionMeta,
    ) -> Result<(), WorkerLogError> {
        let session_dir = self.session_directory(worker_session_id);
        let temp_path = session_dir.join(format!("{META_FILE}{META_TMP_SUFFIX}"));
        let encoded =
            serde_json::to_string(meta).map_err(|_| WorkerLogError::io("meta encoding"))?;
        write_private_file(&temp_path, &encoded)?;
        fs::rename(&temp_path, session_dir.join(META_FILE))
            .map_err(|_| WorkerLogError::io("meta publish"))
    }
}

fn validate_config(config: &WorkerLogConfig) -> Result<(), WorkerLogError> {
    if config.max_diagnostic_line_bytes < MIN_DIAGNOSTIC_LINE_BYTES
        || config.max_diagnostic_line_bytes > MAX_DIAGNOSTIC_LINE_BYTES
    {
        return Err(WorkerLogError::invalid(format!(
            "max diagnostic line bytes must be within \
             {MIN_DIAGNOSTIC_LINE_BYTES}..={MAX_DIAGNOSTIC_LINE_BYTES}"
        )));
    }
    if config.max_segment_bytes
        < u64::try_from(config.max_diagnostic_line_bytes).unwrap_or(u64::MAX)
        || config.max_segment_bytes > MAX_SEGMENT_BYTES
    {
        return Err(WorkerLogError::invalid(format!(
            "max segment bytes must be within the line cap..={MAX_SEGMENT_BYTES}"
        )));
    }
    if config.max_segments_per_stream == 0
        || config.max_segments_per_stream > MAX_SEGMENTS_PER_STREAM
    {
        return Err(WorkerLogError::invalid(format!(
            "max segments per stream must be within 1..={MAX_SEGMENTS_PER_STREAM}"
        )));
    }
    if config.max_retained_sessions == 0 || config.max_retained_sessions > MAX_RETAINED_SESSIONS {
        return Err(WorkerLogError::invalid(format!(
            "max retained sessions must be within 1..={MAX_RETAINED_SESSIONS}"
        )));
    }
    Ok(())
}

fn validate_session_id(worker_session_id: &str) -> Result<(), WorkerLogError> {
    let valid = !worker_session_id.is_empty()
        && worker_session_id.len() <= MAX_SESSION_ID_BYTES
        && !worker_session_id.starts_with('.')
        && worker_session_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'));
    if valid {
        Ok(())
    } else {
        Err(WorkerLogError::invalid(
            "worker session id must be a non-empty single path component of \
             at most 200 alphanumeric, '_', '-', or '.' characters",
        ))
    }
}

fn parse_stamp(stamp: &str) -> Result<OffsetDateTime, WorkerLogError> {
    OffsetDateTime::parse(stamp, &Rfc3339)
        .map_err(|_| WorkerLogError::invalid("stamp must be an RFC 3339 timestamp"))
}

fn create_private_directory(path: &Path) -> Result<(), WorkerLogError> {
    fs::create_dir_all(path).map_err(|_| WorkerLogError::io("directory create"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let mut permissions = fs::metadata(path)
            .map_err(|_| WorkerLogError::io("directory metadata"))?
            .permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(path, permissions)
            .map_err(|_| WorkerLogError::io("directory permissions"))?;
    }
    Ok(())
}

fn write_private_file(path: &Path, contents: &str) -> Result<(), WorkerLogError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        use std::os::unix::fs::PermissionsExt;

        let mut file = fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(path)
            .map_err(|_| WorkerLogError::io("private file create"))?;
        file.write_all(contents.as_bytes())
            .map_err(|_| WorkerLogError::io("private file write"))?;
        let mut permissions = file
            .metadata()
            .map_err(|_| WorkerLogError::io("private file metadata"))?
            .permissions();
        permissions.set_mode(0o600);
        fs::set_permissions(path, permissions)
            .map_err(|_| WorkerLogError::io("private file permissions"))?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        fs::write(path, contents).map_err(|_| WorkerLogError::io("private file write"))
    }
}

fn write_line_to_stream(
    session_dir: &Path,
    stem: &str,
    line: &str,
    config: &WorkerLogConfig,
) -> Result<u64, WorkerLogError> {
    let active = session_dir.join(format!("{stem}.log"));
    let current = fs::metadata(&active).map_or(0, |metadata| metadata.len());
    let mut rotations = 0;
    let line_bytes = u64::try_from(line.len())
        .unwrap_or(u64::MAX)
        .saturating_add(1);
    if current > 0 && current + line_bytes > config.max_segment_bytes {
        rotate_stream(session_dir, stem, config)?;
        rotations = 1;
    }
    append_segment_line(&active, line)?;
    Ok(rotations)
}

fn append_segment_line(active: &Path, line: &str) -> Result<(), WorkerLogError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;

        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(active)
            .map_err(|_| WorkerLogError::io("segment open"))?;
        write_line_bytes(&mut file, line)
    }
    #[cfg(not(unix))]
    {
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(active)
            .map_err(|_| WorkerLogError::io("segment open"))?;
        write_line_bytes(&mut file, line)
    }
}

fn write_line_bytes(file: &mut fs::File, line: &str) -> Result<(), WorkerLogError> {
    file.write_all(line.as_bytes())
        .and_then(|()| file.write_all(b"\n"))
        .map_err(|_| WorkerLogError::io("segment append"))
}

fn rotate_stream(
    session_dir: &Path,
    stem: &str,
    config: &WorkerLogConfig,
) -> Result<(), WorkerLogError> {
    let active = session_dir.join(format!("{stem}.log"));
    if config.max_segments_per_stream == 1 {
        fs::remove_file(&active).map_err(|_| WorkerLogError::io("segment rotation"))?;
        return Ok(());
    }
    let oldest = session_dir.join(format!("{stem}.{}.log", config.max_segments_per_stream - 1));
    let _ = fs::remove_file(&oldest);
    for index in (1..config.max_segments_per_stream - 1).rev() {
        let _ = fs::rename(
            session_dir.join(format!("{stem}.{index}.log")),
            session_dir.join(format!("{stem}.{}.log", index + 1)),
        );
    }
    fs::rename(&active, session_dir.join(format!("{stem}.1.log")))
        .map_err(|_| WorkerLogError::io("segment rotation"))
}

fn retained_stream_bytes(session_dir: &Path, stem: &str, max_segments: usize) -> u64 {
    (0..max_segments)
        .map(|index| {
            let name = if index == 0 {
                format!("{stem}.log")
            } else {
                format!("{stem}.{index}.log")
            };
            fs::metadata(session_dir.join(name)).map_or(0, |metadata| metadata.len())
        })
        .sum()
}

fn remove_session_directory(
    logs_root: &Path,
    worker_session_id: &str,
) -> Result<(), WorkerLogError> {
    fs::remove_dir_all(logs_root.join(worker_session_id))
        .map_err(|_| WorkerLogError::io("session prune"))
}

fn apply_line(meta: &mut SessionMeta, stats: &mut WorkerLogAppendStats, processed: &ProcessedLine) {
    let omitted = u64::from(processed.output.is_none());
    let credential = u64::from(processed.credential_redacted);
    meta.lines_omitted_oversize = meta.lines_omitted_oversize.saturating_add(omitted);
    meta.lines_redacted_credential = meta.lines_redacted_credential.saturating_add(credential);
    meta.absolute_paths_masked = meta
        .absolute_paths_masked
        .saturating_add(processed.paths_masked);
    meta.control_bytes_stripped = meta
        .control_bytes_stripped
        .saturating_add(processed.control_bytes_stripped);
    stats.lines_omitted_oversize += omitted;
    stats.lines_redacted_credential += credential;
    stats.absolute_paths_masked += processed.paths_masked;
    stats.control_bytes_stripped += processed.control_bytes_stripped;
}

fn process_line(line: &str, max_line_bytes: usize) -> ProcessedLine {
    let mut text = String::with_capacity(line.len());
    let mut control_bytes = 0u64;
    for character in line.chars() {
        if character != '\t' && character.is_control() {
            control_bytes += 1;
            continue;
        }
        text.push(character);
    }
    if text.len() > max_line_bytes {
        return ProcessedLine {
            output: None,
            credential_redacted: false,
            paths_masked: 0,
            control_bytes_stripped: control_bytes,
        };
    }
    if contains_credential_material(&text) {
        return ProcessedLine {
            output: Some(CREDENTIAL_MARKER.to_owned()),
            credential_redacted: true,
            paths_masked: 0,
            control_bytes_stripped: control_bytes,
        };
    }
    let (masked, paths) = mask_absolute_paths(&text);
    ProcessedLine {
        output: Some(masked),
        credential_redacted: false,
        paths_masked: paths,
        control_bytes_stripped: control_bytes,
    }
}

const CREDENTIAL_MARKERS: &[&str] = &[
    "--api-key ",
    "--password ",
    "--secret ",
    "--token ",
    "api_key=",
    "api-key:",
    "apikey=",
    "authorization:",
    "authorization=",
    "aws_secret_access_key",
    "bearer ",
    "credential=",
    "gho_",
    "ghp_",
    "ghr_",
    "ghs_",
    "ghu_",
    "github_pat_",
    "glpat-",
    "-----begin private key-----",
    "-----begin rsa private key-----",
    "-----begin ecdsa private key-----",
    "-----begin openssh private key-----",
    "password=",
    "private key",
    "secret=",
    "set-cookie:",
    "sk-ant-",
    "sk-proj-",
    "sk-svcacct-",
    "token=",
    "wsc-",
    "wsc_",
    "workercredential",
    "x-api-key:",
    "x-app-",
    "xapp-",
    "xoxb-",
    "xoxp-",
];

fn contains_credential_material(line: &str) -> bool {
    let lowered = line.to_ascii_lowercase();
    CREDENTIAL_MARKERS
        .iter()
        .any(|marker| lowered.contains(marker))
        || contains_long_hex(&lowered)
        || contains_url_userinfo(&lowered)
}

fn contains_long_hex(line: &str) -> bool {
    let mut run = 0usize;
    for byte in line.bytes() {
        if byte.is_ascii_hexdigit() {
            run += 1;
            if run >= 32 {
                return true;
            }
        } else {
            run = 0;
        }
    }
    false
}

fn contains_url_userinfo(line: &str) -> bool {
    let mut remainder = line;
    while let Some(scheme_end) = remainder.find("://") {
        let after_scheme = &remainder[scheme_end + 3..];
        let authority_end = after_scheme
            .find(|character: char| character.is_ascii_whitespace() || "/?#".contains(character))
            .unwrap_or(after_scheme.len());
        let authority = &after_scheme[..authority_end];
        if authority
            .rfind('@')
            .is_some_and(|at| authority[..at].contains(':'))
        {
            return true;
        }
        remainder = &after_scheme[authority_end..];
    }
    false
}

fn mask_absolute_paths(line: &str) -> (String, u64) {
    let bytes = line.as_bytes();
    let mut masked = String::with_capacity(line.len());
    let mut count = 0u64;
    let mut position = 0usize;
    let mut previous: Option<char> = None;
    while position < bytes.len() {
        let byte = bytes[position];

        if matches!(byte, b'f' | b'F')
            && line[position..].len() >= 7
            && line[position..position + 7].eq_ignore_ascii_case("file://")
        {
            let end = url_end(bytes, position + 7);
            masked.push_str(PATH_MARKER);
            count += 1;
            previous = Some(char::from(bytes[end - 1]));
            position = end;
            continue;
        }
        if byte == b'\\'
            && position + 2 < bytes.len()
            && bytes[position + 1] == b'\\'
            && bytes[position + 2] != b'\\'
            && is_path_boundary(previous)
        {
            let end = consume_windows_run(bytes, position);
            if end > position + 2 {
                masked.push_str(PATH_MARKER);
                count += 1;
                previous = Some(char::from(bytes[end - 1]));
                position = end;
                continue;
            }
        }
        if byte.is_ascii_alphabetic()
            && position + 2 < bytes.len()
            && bytes[position + 1] == b':'
            && matches!(bytes[position + 2], b'\\' | b'/')
            && is_path_boundary(previous)
        {
            let end = consume_windows_run(bytes, position + 2);
            if end >= position + 4 {
                masked.push_str(PATH_MARKER);
                count += 1;
                previous = Some(char::from(bytes[end - 1]));
                position = end;
                continue;
            }
        }
        if byte == b'~'
            && position + 1 < bytes.len()
            && bytes[position + 1] == b'/'
            && is_path_boundary(previous)
        {
            let end = consume_posix_run(bytes, position + 1);
            if end > position + 2 {
                masked.push_str(PATH_MARKER);
                count += 1;
                previous = Some(char::from(bytes[end - 1]));
                position = end;
                continue;
            }
        }
        if byte == b'/'
            && position + 1 < bytes.len()
            && bytes[position + 1] != b'/'
            && is_path_boundary(previous)
        {
            let end = consume_posix_run(bytes, position);
            let run = &line[position..end];
            if run.len() >= 2 && run[1..].chars().any(|character| character != '/') {
                masked.push_str(PATH_MARKER);
                count += 1;
                previous = Some(char::from(bytes[end - 1]));
                position = end;
                continue;
            }
        }

        let character = line[position..]
            .chars()
            .next()
            .unwrap_or(char::REPLACEMENT_CHARACTER);
        masked.push(character);
        previous = Some(character);
        position += character.len_utf8().max(1);
    }
    (masked, count)
}

fn is_path_boundary(previous: Option<char>) -> bool {
    match previous {
        None => true,
        Some(character) => {
            !character.is_ascii_alphanumeric() && !matches!(character, '/' | '\\' | '.' | '_' | '-')
        }
    }
}

fn url_end(bytes: &[u8], start: usize) -> usize {
    let mut end = start;
    while end < bytes.len() {
        let byte = bytes[end];
        if byte.is_ascii_whitespace() || matches!(byte, b'"' | b'\'' | b'<' | b'>' | b')' | b']') {
            break;
        }
        end += 1;
    }
    end
}

fn consume_posix_run(bytes: &[u8], start: usize) -> usize {
    let mut end = start;
    while end < bytes.len()
        && (bytes[end].is_ascii_alphanumeric()
            || matches!(bytes[end], b'/' | b'.' | b'_' | b'-')
            || bytes[end] >= 0x80)
    {
        end += 1;
    }
    end
}

fn consume_windows_run(bytes: &[u8], start: usize) -> usize {
    let mut end = start;
    while end < bytes.len()
        && (bytes[end].is_ascii_alphanumeric()
            || matches!(bytes[end], b'\\' | b'/' | b'.' | b'_' | b'-')
            || bytes[end] >= 0x80)
    {
        end += 1;
    }
    end
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credential_markers_are_detected_case_insensitively() {
        for line in [
            "Authorization: Bearer abc",
            "export GITHUB_TOKEN=ghp_short",
            "--token s3cr3t",
            "wsc-live-material-01",
            "-----BEGIN PRIVATE KEY-----",
            "https://alice:hunter2@example.invalid/repo.git",
            "api_key=abc",
            "Set-Cookie: session=1",
        ] {
            assert!(
                contains_credential_material(line),
                "expected credential detection for: {line}"
            );
        }
        assert!(!contains_credential_material("plain diagnostics line"));
        assert!(!contains_credential_material("digest deadbeef done"));
    }

    #[test]
    fn long_hexadecimal_runs_are_treated_as_credential_material() {
        assert!(contains_credential_material(&"a".repeat(64)));
        assert!(contains_credential_material(
            "digest sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef ok"
        ));
        assert!(!contains_credential_material("digest deadbeef done"));
    }

    #[test]
    fn absolute_paths_are_masked_selectively() {
        let (masked, count) =
            mask_absolute_paths("failed to read /Users/alice/secret-project/token.txt");
        assert_eq!(count, 1);
        assert_eq!(masked, "failed to read [PATH]");

        let (masked, count) = mask_absolute_paths("PATH=/usr/local/bin:/usr/bin");
        assert_eq!(count, 2);
        assert_eq!(masked, "PATH=[PATH]:[PATH]");

        let (masked, count) =
            mask_absolute_paths("config at C:\\Users\\bob\\secret.txt and \\\\srv\\share\\k.pem");
        assert_eq!(count, 2);
        assert!(!masked.contains("bob"));
        assert!(!masked.contains("srv"));

        let (masked, count) = mask_absolute_paths("see file:///home/carol/secret.txt please");
        assert_eq!(count, 1);
        assert_eq!(masked, "see [PATH] please");

        let (masked, count) = mask_absolute_paths("notes in ~/private/notes.md");
        assert_eq!(count, 1);
        assert!(!masked.contains("private"));

        let (masked, count) = mask_absolute_paths("https://example.com/foo/bar stayed intact");
        assert_eq!(count, 0);
        assert_eq!(masked, "https://example.com/foo/bar stayed intact");

        let (masked, count) = mask_absolute_paths("relative src/main.rs and a/b stay");
        assert_eq!(count, 0);
        assert_eq!(masked, "relative src/main.rs and a/b stay");

        let (masked, count) = mask_absolute_paths("a lone / and 12:34 stay");
        assert_eq!(count, 0);
        assert_eq!(masked, "a lone / and 12:34 stay");

        let (masked, count) = mask_absolute_paths("时钟 /tmp/时钟.txt ok");
        assert_eq!(count, 1);
        assert_eq!(masked, "时钟 [PATH] ok");
    }

    #[test]
    fn process_line_applies_the_full_pipeline() {
        let processed = process_line("\u{1b}[31mERROR\u{7}: boom /var/db/x", 1_024);
        let output = processed.output.expect("diagnostic line survives");
        assert_eq!(output, "[31mERROR: boom [PATH]");
        assert_eq!(processed.control_bytes_stripped, 2);
        assert_eq!(processed.paths_masked, 1);

        let processed = process_line(&"assistant body ".repeat(200), 1_024);
        assert!(processed.output.is_none());

        let processed = process_line("token ghp_short", 1_024);
        assert_eq!(processed.output.as_deref(), Some(CREDENTIAL_MARKER));
        assert!(processed.credential_redacted);
    }

    #[test]
    fn config_bounds_are_enforced() {
        assert!(validate_config(&WorkerLogConfig::default()).is_ok());
        let below_line_cap = WorkerLogConfig {
            max_segment_bytes: 32,
            ..WorkerLogConfig::default()
        };
        assert!(validate_config(&below_line_cap).is_err());
        let zero_segments = WorkerLogConfig {
            max_segments_per_stream: 0,
            ..WorkerLogConfig::default()
        };
        assert!(validate_config(&zero_segments).is_err());
        let zero_sessions = WorkerLogConfig {
            max_retained_sessions: 0,
            ..WorkerLogConfig::default()
        };
        assert!(validate_config(&zero_sessions).is_err());
        let tiny_line_cap = WorkerLogConfig {
            max_diagnostic_line_bytes: 8,
            ..WorkerLogConfig::default()
        };
        assert!(validate_config(&tiny_line_cap).is_err());
    }

    #[test]
    fn session_ids_must_be_single_safe_components() {
        assert!(validate_session_id("wks_ABCDEF1234567890").is_ok());
        assert!(validate_session_id("session-1.2").is_ok());
        for bad in ["", ".", "..", "a/b", "a\\b", ".hidden", &"x".repeat(201)] {
            assert!(
                validate_session_id(bad).is_err(),
                "expected rejection: {bad}"
            );
        }
    }
}
