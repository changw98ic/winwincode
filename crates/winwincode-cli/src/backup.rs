// SPDX-License-Identifier: Apache-2.0

//! Owner-facing `wwc backup` surface: consistent snapshots, verification,
//! restore, and bounded repair for the Server product-state directory and the
//! Device Client local data directory (OPS-100.5).
//!
//! Four boundaries are deliberate:
//!
//! - Consistency over convenience. A snapshot is produced by `SQLite`'s
//!   `VACUUM INTO` against the live WAL database, so every snapshot is one
//!   consistent committed cut. File-level cold copies of a hot database are
//!   never used as backups; file copies appear only on the restore side,
//!   where the source is an already sealed snapshot file.
//! - No plaintext credential in a backup. The Device Client local store
//!   carries the raw device credential secret by design ("the secret never
//!   leaves the device"), so the snapshot zeroes exactly that column and
//!   keeps the digest-era metadata. Server databases store only Argon2id
//!   hashes and session digests. Every snapshot is additionally scanned for
//!   plaintext credential markers, and verify re-checks the redaction, so a
//!   backup artifact that carries secret material fails verification.
//! - Restore is fail-closed on schema version. Every snapshot's
//!   `PRAGMA user_version` is checked against the same version sets the
//!   canonical adapters accept at startup (aligned with
//!   `startup_rejects_a_database_from_a_newer_schema_version`); an
//!   unsupported version refuses the whole restore before any target file is
//!   touched. Digest, integrity, and secret-scan verification run first, and
//!   the Device credential is re-bound from the live local store by digest,
//!   because the backup deliberately carries no credential material.
//!   Restoring a Device backup therefore requires the same live device; a
//!   lost device must re-enroll instead of restoring identity material.
//! - Repair is bounded and leaves a trace. Without `--apply` it is a
//!   read-only diagnosis (`PRAGMA integrity_check`, schema-version facts,
//!   WAL sizes). `--apply` executes only the allowlisted bounded actions
//!   (WAL checkpoint and stale backup temp-file cleanup); corruption beyond
//!   that is never rewritten — the answer is restore from backup. Every
//!   invocation appends one JSON line to `backup-repair-log.jsonl`.

use std::fmt;
use std::fmt::Write as _;
use std::fs;
use std::io::Write as _;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::cli::WwcCliExit;

/// Success exit code, mirroring the `wwc` command vocabulary.
pub const EXIT_SUCCESS: i32 = 0;
/// Verification findings exit code (the doctor convention).
pub const EXIT_DIAGNOSTIC_FAILED: i32 = 1;
/// Usage error exit code.
pub const EXIT_USAGE: i32 = 2;
/// Store not initialized yet exit code.
pub const EXIT_ACTION_REQUIRED: i32 = 3;
/// Fail-closed refusal or operational failure exit code.
pub const EXIT_SERVICE: i32 = 5;

/// The one accepted local backup manifest format.
const MANIFEST_FORMAT: &str = "winwincode.local-backup.v1";

/// Logical name of the Server product-state database.
const CONTROL_PLANE_DATABASE: &str = "control-plane";
/// Logical name of the Device Client local database.
const DEVICE_CLIENT_DATABASE: &str = "device-client";

/// Current Server product-state schema version, aligned with
/// `winwincode-storage`'s `SCHEMA_VERSION` (6). Locked to the source by
/// `tests/backup-restore.test.mjs`.
const SERVER_CONTROL_PLANE_SCHEMA_VERSION: i64 = 6;
/// Older Server product-state versions `winwincode-storage` still migrates at
/// startup; restore accepts exactly this set plus the current version.
const SERVER_CONTROL_PLANE_MIGRATABLE_VERSIONS: [i64; 5] = [1, 2, 3, 4, 5];

/// Suffix of the intermediate `VACUUM INTO` target, renamed into place only
/// after the snapshot passed every check.
const VACUUM_TMP_SUFFIX: &str = ".vacuum-tmp";
/// Suffix of the intermediate restore copy, renamed into place only after
/// credential re-binding.
const RESTORE_TMP_SUFFIX: &str = ".restore-tmp";
/// The append-only bounded-repair trace file inside the data directory.
const REPAIR_LOG_FILE: &str = "backup-repair-log.jsonl";

/// How long one open or checkpoint waits for a concurrent writer. Matches the
/// storage adapter's bound.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Upper bound on recorded `integrity_check` findings per database so one
/// badly damaged file cannot flood the report or the trace log.
const MAX_FINDINGS_PER_DATABASE: usize = 5;

/// Plaintext credential markers scanned against every snapshot's bytes,
/// conceptually aligned with the evidence-export secret scanner. Hashes and
/// digests (Argon2id PHC strings, `sha256:` digests) are not plaintext and do
/// not match any marker.
const SECRET_MARKERS: &[&str] = &[
    "-----beginprivatekey",
    "authorization:bearer",
    "client_secret",
    "\"password\":\"",
    "\"secret\":\"",
    "ghp_",
    "github_pat_",
    "password=",
    "secret=",
    "sk_live_",
    "wwc_session=",
];

/// Help lines appended to `wwc help` by the CLI dispatcher.
pub const BACKUP_HELP_LINES: [&str; 7] = [
    "  wwc backup snapshot --store server|device --data-dir PATH --output PATH [--json]",
    "  wwc backup verify --from BACKUP-DIR [--json]",
    "  wwc backup restore --store server|device --data-dir PATH --from BACKUP-DIR [--json]",
    "  wwc backup repair --store server|device --data-dir PATH [--apply] [--json]",
    "backup 用 SQLite VACUUM INTO 取一致性快照，绝不用文件冷拷贝热库；备份产物不含明文凭据",
    "（Device 凭据位清零，恢复时按 digest 回绑本机活库凭据，跨设备恢复身份被拒绝）；",
    "restore 校验 schema 版本 fail closed；repair 默认只读诊断，--apply 只执行白名单动作并留痕。",
];

/// Which durable store the command addresses.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BackupStoreKind {
    /// The Server product-state directory (`WWC_SERVER_DATA_DIRECTORY`
    /// semantics): every `*.sqlite3` sidecar database it contains.
    Server,
    /// The Device Client local data directory: `device-client.sqlite3`.
    Device,
}

impl BackupStoreKind {
    /// Parses the `--store` flag value.
    ///
    /// # Errors
    ///
    /// Rejects anything other than `server` or `device`.
    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "server" => Ok(Self::Server),
            "device" => Ok(Self::Device),
            other => Err(format!("--store 只能是 server 或 device，收到 {other}。")),
        }
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Server => "server",
            Self::Device => "device",
        }
    }
}

impl fmt::Display for BackupStoreKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// One snapshot fact recorded in the manifest and the command output.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BackupDatabaseEntry {
    /// Logical database name (the file stem without `.sqlite3`).
    pub name: String,
    /// Canonical snapshot file name inside the backup directory.
    pub file: String,
    /// Raw `PRAGMA user_version` of the snapshot (`0` for sidecars that do
    /// not manage a schema version).
    pub schema_version: i64,
    /// Snapshot byte count at capture time.
    pub byte_count: u64,
    /// `sha256:` digest of the snapshot bytes.
    pub sha256: String,
    /// `PRAGMA quick_check` verdict at capture time.
    pub integrity: String,
    /// Whether the raw device credential column was zeroed in this snapshot.
    pub credential_redacted: bool,
}

/// The only accepted backup manifest document.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BackupManifest {
    /// Manifest format constant.
    pub format: String,
    /// Which store the backup belongs to.
    pub store: String,
    /// Capture wall-clock milliseconds.
    pub created_at_millis: u64,
    /// One entry per snapshotted database.
    pub databases: Vec<BackupDatabaseEntry>,
}

/// One bounded-repair or diagnosis trace line.
#[derive(Clone, Debug, Serialize)]
pub struct RepairLogEntry {
    /// Wall-clock milliseconds of the invocation.
    pub timestamp_millis: u64,
    /// Addressed store.
    pub store: String,
    /// `diagnose` or `apply`.
    pub mode: String,
    /// Read-only findings; empty when healthy.
    pub findings: Vec<RepairFinding>,
    /// Actions actually executed (empty without `--apply`).
    pub actions: Vec<RepairAction>,
}

/// One read-only diagnosis finding.
#[derive(Clone, Debug, Serialize)]
pub struct RepairFinding {
    /// Logical database name.
    pub database: String,
    /// Stable finding code (`open-failed`, `integrity`, `schema-version`).
    pub code: String,
    /// Human-readable detail.
    pub detail: String,
}

/// One executed bounded-repair action.
#[derive(Clone, Debug, Serialize)]
pub struct RepairAction {
    /// Stable action code (`wal-checkpoint`, `stale-temp-cleanup`).
    pub action: String,
    /// Logical target (database name or removed temp file name).
    pub target: String,
    /// `ok` or `failed`.
    pub outcome: String,
    /// Human-readable detail.
    pub detail: String,
}

/// Successful command outcome, rendered human-readable or as JSON.
#[derive(Debug, Serialize)]
#[serde(
    tag = "status",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase"
)]
pub enum BackupOutcome {
    /// One backup directory was produced.
    SnapshotComplete {
        /// Addressed store.
        store: String,
        /// The backup directory holding `manifest.json` and snapshots.
        backup_directory: String,
        /// Per-database facts.
        databases: Vec<BackupDatabaseEntry>,
    },
    /// The backup directory passed every verification.
    VerifyOk {
        /// Addressed store recorded in the manifest.
        store: String,
        /// Per-database re-verified facts.
        databases: Vec<BackupDatabaseEntry>,
    },
    /// One restore completed and placed every snapshot.
    RestoreComplete {
        /// Addressed store.
        store: String,
        /// Restored database names.
        databases: Vec<String>,
        /// Restart guidance for the owning process.
        note: String,
    },
    /// One diagnosis or bounded repair completed.
    RepairReport {
        /// `diagnose` or `apply`.
        mode: String,
        /// Whether every database passed diagnosis.
        healthy: bool,
        /// Read-only findings.
        findings: Vec<RepairFinding>,
        /// Actions executed (only with `--apply`).
        actions: Vec<RepairAction>,
        /// The trace file the entry was appended to.
        log_file: String,
    },
}

/// Failure of one backup command.
#[derive(Clone, Debug)]
pub enum BackupFailure {
    /// Invalid command usage; rendered with the backup usage text.
    Usage {
        /// Human-readable explanation in the CLI language.
        message: String,
    },
    /// The addressed data directory holds no database to operate on.
    NotInitialized {
        /// Human-readable explanation in the CLI language.
        message: String,
    },
    /// Verification found a fact that contradicts the manifest or the
    /// secret-free contract. Rendered like a failed doctor run.
    VerifyFailed {
        /// Bounded list of findings.
        findings: Vec<String>,
    },
    /// Fail-closed refusal with a stable machine-readable code.
    Refused {
        /// Stable code (`restore.unsupported-schema-version`,
        /// `backup.secret-detected`, ...).
        code: &'static str,
        /// Human-readable explanation in the CLI language.
        message: String,
    },
    /// Operational failure (I/O, `SQLite`) with a stable code.
    Failed {
        /// Stable code.
        code: &'static str,
        /// Human-readable explanation in the CLI language.
        message: String,
    },
}

impl fmt::Display for BackupFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Usage { message }
            | Self::NotInitialized { message }
            | Self::Refused { message, .. }
            | Self::Failed { message, .. } => formatter.write_str(message),
            Self::VerifyFailed { findings } => {
                write!(formatter, "校验失败：{}", findings.join("；"))
            }
        }
    }
}

struct BackupCommandExit {
    outcome: BackupOutcome,
    code: i32,
    json: bool,
}

/// Runs one `wwc backup ...` command. Usage failures are rendered with the
/// backup usage text, mirroring the `run_cli` convention.
#[must_use]
pub fn run_backup(arguments: &[String]) -> WwcCliExit {
    match run_backup_inner(arguments) {
        Ok(command) => {
            let stdout = if command.json {
                let json = serde_json::to_string_pretty(&command.outcome)
                    .unwrap_or_else(|_| "{}".to_owned());
                format!("{json}\n")
            } else {
                render_plain(&command.outcome)
            };
            WwcCliExit {
                code: command.code,
                stdout,
                stderr: String::new(),
            }
        }
        Err(failure) => {
            let (code, message) = render_failure(&failure);
            WwcCliExit {
                code,
                stdout: String::new(),
                stderr: format!("backup 命令失败：{message}\n"),
            }
        }
    }
}

fn run_backup_inner(arguments: &[String]) -> Result<BackupCommandExit, BackupFailure> {
    let Some(action) = arguments.first().map(String::as_str) else {
        return Err(usage_failure(
            "backup 后需要 snapshot、verify、restore 或 repair。",
        ));
    };
    let rest = &arguments[1..];
    match action {
        "snapshot" => run_snapshot(rest),
        "verify" => run_verify(rest),
        "restore" => run_restore(rest),
        "repair" => run_repair(rest),
        other => Err(usage_failure(&format!("未知 backup 命令 {other}。"))),
    }
}

fn usage_failure(message: &str) -> BackupFailure {
    BackupFailure::Usage {
        message: format!("{message}\n\n{}", BACKUP_HELP_LINES.join("\n")),
    }
}

struct ParsedArguments {
    flags: Vec<(String, String)>,
    switches: Vec<String>,
}

fn parse_arguments(arguments: &[String]) -> Result<ParsedArguments, BackupFailure> {
    let mut parsed = ParsedArguments {
        flags: Vec::new(),
        switches: Vec::new(),
    };
    let mut index = 0;
    while index < arguments.len() {
        let token = &arguments[index];
        if !token.starts_with("--") {
            return Err(usage_failure(&format!("backup 不接受位置参数 {token}。")));
        }
        let option = &token[2..];
        if option.is_empty() {
            return Err(usage_failure("选项名称不能为空。"));
        }
        if let Some((name, value)) = option.split_once('=') {
            parsed.flags.push((name.to_owned(), value.to_owned()));
            index += 1;
            continue;
        }
        let value = arguments.get(index + 1);
        match value {
            Some(value) if !value.starts_with("--") => {
                parsed.flags.push((option.to_owned(), value.clone()));
                index += 2;
            }
            _ => {
                parsed.switches.push(option.to_owned());
                index += 1;
            }
        }
    }
    Ok(parsed)
}

fn reject_unknown(
    parsed: &ParsedArguments,
    flags: &[&str],
    switches: &[&str],
) -> Result<(), BackupFailure> {
    for (name, _) in &parsed.flags {
        if !flags.contains(&name.as_str()) {
            return Err(usage_failure(&format!("未知选项 --{name}。")));
        }
    }
    for name in &parsed.switches {
        if !switches.contains(&name.as_str()) {
            return Err(usage_failure(&format!("未知选项 --{name}。")));
        }
    }
    Ok(())
}

fn single_flag(parsed: &ParsedArguments, name: &str) -> Result<Option<String>, BackupFailure> {
    let matches: Vec<&String> = parsed
        .flags
        .iter()
        .filter_map(|(flag, value)| (flag == name).then_some(value))
        .collect();
    if matches.len() > 1 {
        return Err(usage_failure(&format!("--{name} 不能重复。")));
    }
    Ok(matches.first().map(|value| (*value).clone()))
}

fn require_flag(parsed: &ParsedArguments, name: &str) -> Result<String, BackupFailure> {
    match single_flag(parsed, name)? {
        Some(value) => Ok(value),
        None => Err(usage_failure(&format!("缺少 --{name}。"))),
    }
}

fn has_switch(parsed: &ParsedArguments, name: &str) -> bool {
    parsed.switches.iter().any(|switch| switch == name)
}

fn parse_store(parsed: &ParsedArguments) -> Result<BackupStoreKind, BackupFailure> {
    let value = require_flag(parsed, "store")?;
    BackupStoreKind::parse(&value).map_err(|message| usage_failure(&message))
}

/// Resolves the data directory. The Server store falls back to
/// `WWC_SERVER_DATA_DIRECTORY` exactly like the `wwc user` commands; the
/// Device store requires an explicit `--data-dir` like the `wwc device`
/// commands.
fn resolve_data_directory(
    parsed: &ParsedArguments,
    store: BackupStoreKind,
) -> Result<PathBuf, BackupFailure> {
    if let Some(value) = single_flag(parsed, "data-dir")? {
        return Ok(PathBuf::from(value));
    }
    if store == BackupStoreKind::Server
        && let Some(value) = std::env::var_os("WWC_SERVER_DATA_DIRECTORY")
    {
        return Ok(PathBuf::from(value));
    }
    Err(usage_failure(
        "缺少 --data-dir：backup 命令需要数据目录（与对应进程的数据目录一致）。",
    ))
}

fn run_snapshot(arguments: &[String]) -> Result<BackupCommandExit, BackupFailure> {
    let parsed = parse_arguments(arguments)?;
    reject_unknown(&parsed, &["store", "data-dir", "output"], &["json"])?;
    let json = has_switch(&parsed, "json");
    let store = parse_store(&parsed)?;
    let data_directory = resolve_data_directory(&parsed, store)?;
    let output = PathBuf::from(require_flag(&parsed, "output")?);
    let manifest = snapshot_store(store, &data_directory, &output)?;
    Ok(BackupCommandExit {
        outcome: BackupOutcome::SnapshotComplete {
            store: store.as_str().to_owned(),
            backup_directory: output.display().to_string(),
            databases: manifest.databases,
        },
        code: EXIT_SUCCESS,
        json,
    })
}

fn run_verify(arguments: &[String]) -> Result<BackupCommandExit, BackupFailure> {
    let parsed = parse_arguments(arguments)?;
    reject_unknown(&parsed, &["from"], &["json"])?;
    let json = has_switch(&parsed, "json");
    let from = PathBuf::from(require_flag(&parsed, "from")?);
    let manifest = verify_backup_directory(&from)?;
    Ok(BackupCommandExit {
        outcome: BackupOutcome::VerifyOk {
            store: manifest.store,
            databases: manifest.databases,
        },
        code: EXIT_SUCCESS,
        json,
    })
}

fn run_restore(arguments: &[String]) -> Result<BackupCommandExit, BackupFailure> {
    let parsed = parse_arguments(arguments)?;
    reject_unknown(&parsed, &["store", "data-dir", "from"], &["json"])?;
    let json = has_switch(&parsed, "json");
    let store = parse_store(&parsed)?;
    let data_directory = resolve_data_directory(&parsed, store)?;
    let from = PathBuf::from(require_flag(&parsed, "from")?);
    let restored = restore_store(store, &data_directory, &from)?;
    Ok(BackupCommandExit {
        outcome: BackupOutcome::RestoreComplete {
            store: store.as_str().to_owned(),
            databases: restored,
            note: "恢复已落盘；正在运行中的 Server / Device Client 需重启后才读新库。".to_owned(),
        },
        code: EXIT_SUCCESS,
        json,
    })
}

fn run_repair(arguments: &[String]) -> Result<BackupCommandExit, BackupFailure> {
    let parsed = parse_arguments(arguments)?;
    reject_unknown(&parsed, &["store", "data-dir"], &["apply", "json"])?;
    let json = has_switch(&parsed, "json");
    let store = parse_store(&parsed)?;
    let data_directory = resolve_data_directory(&parsed, store)?;
    let apply = has_switch(&parsed, "apply");
    let report = repair_store(store, &data_directory, apply)?;
    let healthy = report.healthy;
    Ok(BackupCommandExit {
        outcome: BackupOutcome::RepairReport {
            mode: if apply {
                "apply".to_owned()
            } else {
                "diagnose".to_owned()
            },
            healthy,
            findings: report.findings,
            actions: report.actions,
            log_file: data_directory.join(REPAIR_LOG_FILE).display().to_string(),
        },
        code: if healthy {
            EXIT_SUCCESS
        } else {
            EXIT_DIAGNOSTIC_FAILED
        },
        json,
    })
}

struct RepairReport {
    healthy: bool,
    findings: Vec<RepairFinding>,
    actions: Vec<RepairAction>,
}

// ---------------------------------------------------------------------------
// Snapshot
// ---------------------------------------------------------------------------

/// Discovers the databases one store owns inside its data directory.
///
/// Server discovery is intentionally dynamic: every `*.sqlite3` sidecar the
/// data directory contains is covered, so new sidecars are backed up without
/// this module knowing them. Device discovery is the canonical single store.
fn discover_databases(
    store: BackupStoreKind,
    data_directory: &Path,
) -> Result<Vec<(String, PathBuf)>, BackupFailure> {
    let entries = match store {
        BackupStoreKind::Device => vec![(
            DEVICE_CLIENT_DATABASE.to_owned(),
            data_directory.join(format!("{DEVICE_CLIENT_DATABASE}.sqlite3")),
        )],
        BackupStoreKind::Server => {
            let read = fs::read_dir(data_directory).map_err(|_| BackupFailure::NotInitialized {
                message: format!(
                    "Server 数据目录不存在或不可读：{}",
                    data_directory.display()
                ),
            })?;
            let mut discovered = Vec::new();
            for entry in read.flatten() {
                let file_name = entry.file_name();
                let Some(name) = file_name.to_str() else {
                    continue;
                };
                let Some(stem) = name.strip_suffix(".sqlite3") else {
                    continue;
                };
                if !entry.path().is_file() {
                    continue;
                }
                discovered.push((stem.to_owned(), entry.path()));
            }
            discovered.sort_by(|first, second| first.0.cmp(&second.0));
            discovered
        }
    };
    let initialized = !entries.is_empty() && entries.iter().all(|(_, path)| path.is_file());
    if !initialized {
        return Err(BackupFailure::NotInitialized {
            message: format!(
                "{} 尚未初始化：{} 中没有可备份的 SQLite 数据库。",
                store_display(store),
                data_directory.display()
            ),
        });
    }
    Ok(entries)
}

fn store_display(store: BackupStoreKind) -> &'static str {
    match store {
        BackupStoreKind::Server => "Server",
        BackupStoreKind::Device => "Device Client",
    }
}

/// Supported-restore version set for one logical database. `None` marks a
/// sidecar that does not manage `user_version` (integrity-only restore).
fn supported_restore_versions(name: &str) -> Option<Vec<i64>> {
    match name {
        CONTROL_PLANE_DATABASE => {
            let mut supported = SERVER_CONTROL_PLANE_MIGRATABLE_VERSIONS.to_vec();
            supported.push(SERVER_CONTROL_PLANE_SCHEMA_VERSION);
            Some(supported)
        }
        DEVICE_CLIENT_DATABASE => Some(vec![winwincode_device_client::CLIENT_STORE_SCHEMA_VERSION]),
        _ => None,
    }
}

/// The human label of the version-gated databases, `None` for sidecars.
fn schema_gate_label(name: &str) -> Option<&'static str> {
    match name {
        CONTROL_PLANE_DATABASE => Some("Server 产品状态库"),
        DEVICE_CLIENT_DATABASE => Some("Device Client 本地库"),
        _ => None,
    }
}

fn snapshot_store(
    store: BackupStoreKind,
    data_directory: &Path,
    output: &Path,
) -> Result<BackupManifest, BackupFailure> {
    let databases = discover_databases(store, data_directory)?;
    prepare_output_directory(data_directory, output)?;
    let mut entries = Vec::new();
    for (name, source) in databases {
        let snapshot_path = output.join(format!("{name}.sqlite3"));
        let vacuum_tmp = output.join(format!("{name}.sqlite3{VACUUM_TMP_SUFFIX}"));
        let schema_version = create_snapshot(&source, &vacuum_tmp)
            .map_err(|failure| cleanup_failed_snapshot(failure, &vacuum_tmp))?;
        let credential_redacted = name == DEVICE_CLIENT_DATABASE;
        if credential_redacted {
            redact_device_credentials(&vacuum_tmp)
                .map_err(|failure| cleanup_failed_snapshot(failure, &vacuum_tmp))?;
        }
        let entry = seal_snapshot(
            &vacuum_tmp,
            &snapshot_path,
            &name,
            schema_version,
            credential_redacted,
        )
        .map_err(|failure| cleanup_failed_snapshot(failure, &vacuum_tmp))?;
        entries.push(entry);
    }
    let manifest = BackupManifest {
        format: MANIFEST_FORMAT.to_owned(),
        store: store.as_str().to_owned(),
        created_at_millis: now_millis()?,
        databases: entries.clone(),
    };
    write_manifest(output, &manifest)?;
    Ok(manifest)
}

fn cleanup_failed_snapshot(failure: BackupFailure, vacuum_tmp: &Path) -> BackupFailure {
    let _ = fs::remove_file(vacuum_tmp);
    failure
}

fn prepare_output_directory(data_directory: &Path, output: &Path) -> Result<(), BackupFailure> {
    let data_absolute =
        fs::canonicalize(data_directory).map_err(|error| BackupFailure::Failed {
            code: "backup.data-directory",
            message: format!("数据目录不可用：{error}"),
        })?;
    let output_absolute = canonical_existing_or_parent(output);
    if output_absolute
        .as_deref()
        .is_some_and(|path| path.starts_with(&data_absolute))
    {
        return Err(BackupFailure::Refused {
            code: "backup.output-inside-data-directory",
            message: format!(
                "备份目录不能位于数据目录内（{} 在 {} 下）：否则备份产物会被当作业务库再次备份。",
                output.display(),
                data_absolute.display()
            ),
        });
    }
    if output.exists() {
        let empty = fs::read_dir(output)
            .map_err(|error| BackupFailure::Failed {
                code: "backup.output-directory",
                message: format!("备份目录不可读：{error}"),
            })?
            .next()
            .is_none();
        if !empty {
            return Err(BackupFailure::Refused {
                code: "backup.output-not-empty",
                message: format!(
                    "备份目录已存在且非空：{}；请指定一个新目录，避免覆盖既有备份。",
                    output.display()
                ),
            });
        }
    } else {
        fs::create_dir_all(output).map_err(|error| BackupFailure::Failed {
            code: "backup.output-directory",
            message: format!("无法创建备份目录：{error}"),
        })?;
    }
    set_directory_permissions(output)?;
    Ok(())
}

/// Creates one consistent snapshot of one live database with `VACUUM INTO`.
///
/// The source connection never issues DML; it is opened read-write only to
/// avoid `SQLite`'s read-only WAL shared-memory limitation on a live store.
/// `VACUUM INTO` reads one committed cut through the WAL and refuses to
/// overwrite an existing target, so the intermediate file cannot clobber
/// anything.
fn create_snapshot(source: &Path, vacuum_tmp: &Path) -> Result<i64, BackupFailure> {
    let connection = open_live_connection(source)?;
    let check: String = connection
        .query_row("PRAGMA quick_check", [], |row| row.get(0))
        .map_err(|error| BackupFailure::Failed {
            code: "backup.quick-check",
            message: format!("{} quick_check 失败：{error}", source.display()),
        })?;
    if check != "ok" {
        return Err(BackupFailure::Refused {
            code: "backup.unhealthy-source",
            message: format!(
                "{} 完整性检查未通过（{check}）；先诊断修复，不要备份损坏状态。",
                source.display()
            ),
        });
    }
    let schema_version: i64 = connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .map_err(|error| BackupFailure::Failed {
            code: "backup.user-version",
            message: format!("{} 读取 schema 版本失败：{error}", source.display()),
        })?;
    connection
        .execute("VACUUM INTO ?1", params![vacuum_tmp.display().to_string()])
        .map_err(|error| BackupFailure::Failed {
            code: "backup.vacuum-into",
            message: format!("{} 一致性快照失败：{error}", source.display()),
        })?;
    drop(connection);
    Ok(schema_version)
}

/// Zeroes the raw device credential secret inside one Device snapshot. The
/// digest-era metadata (digest, generation) stays, so verify and restore can
/// bind the snapshot to the live credential without carrying the secret.
fn redact_device_credentials(snapshot_tmp: &Path) -> Result<(), BackupFailure> {
    let connection = open_live_connection(snapshot_tmp)?;
    let credential_table: Option<String> = connection
        .query_row(
            "SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'device_credential'",
            [],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| BackupFailure::Failed {
            code: "backup.redaction",
            message: format!("读取快照 schema 失败：{error}"),
        })?;
    if credential_table.is_some() {
        connection
            .execute(
                "UPDATE device_credential SET credential_secret = zeroblob(32)",
                [],
            )
            .map_err(|error| BackupFailure::Failed {
                code: "backup.redaction",
                message: format!("快照凭据清零失败：{error}"),
            })?;
    }
    drop(connection);
    Ok(())
}

/// Verifies and seals one snapshot file: integrity, secret scan, digest,
/// owner-only permissions, atomic rename into its final name.
fn seal_snapshot(
    vacuum_tmp: &Path,
    snapshot_path: &Path,
    name: &str,
    schema_version: i64,
    credential_redacted: bool,
) -> Result<BackupDatabaseEntry, BackupFailure> {
    let integrity = verify_snapshot_file(vacuum_tmp, name)?;
    let bytes = fs::read(vacuum_tmp).map_err(|error| BackupFailure::Failed {
        code: "backup.snapshot-read",
        message: format!("快照读取失败：{error}"),
    })?;
    if let Some(marker) = scan_secret_markers(&bytes) {
        return Err(BackupFailure::Refused {
            code: "backup.secret-detected",
            message: format!("{name} 快照包含明文凭据标记（{marker}）；快照已拒绝。"),
        });
    }
    set_owner_only_permissions(vacuum_tmp)?;
    fs::rename(vacuum_tmp, snapshot_path).map_err(|error| BackupFailure::Failed {
        code: "backup.snapshot-rename",
        message: format!("快照落盘失败：{error}"),
    })?;
    Ok(BackupDatabaseEntry {
        name: name.to_owned(),
        file: snapshot_path.file_name().map_or_else(
            || name.to_owned(),
            |file| file.to_string_lossy().into_owned(),
        ),
        schema_version,
        byte_count: bytes.len() as u64,
        sha256: sha256_digest(&bytes),
        integrity,
        credential_redacted,
    })
}

/// Verifies one sealed snapshot file and returns its `quick_check` verdict.
fn verify_snapshot_file(path: &Path, name: &str) -> Result<String, BackupFailure> {
    let connection = open_snapshot_connection(path)?;
    let check: String = connection
        .query_row("PRAGMA quick_check", [], |row| row.get(0))
        .map_err(|error| BackupFailure::Failed {
            code: "backup.snapshot-quick-check",
            message: format!("{name} 快照完整性检查失败：{error}"),
        })?;
    drop(connection);
    if check != "ok" {
        return Err(BackupFailure::Refused {
            code: "backup.snapshot-unhealthy",
            message: format!("{name} 快照完整性未通过（{check}）；快照已拒绝。"),
        });
    }
    Ok(check)
}

fn write_manifest(output: &Path, manifest: &BackupManifest) -> Result<(), BackupFailure> {
    let bytes = serde_json::to_vec_pretty(manifest).map_err(|error| BackupFailure::Failed {
        code: "backup.manifest-encode",
        message: format!("manifest 编码失败：{error}"),
    })?;
    let manifest_tmp = output.join(format!("manifest.json{VACUUM_TMP_SUFFIX}"));
    {
        let mut file = fs::File::create(&manifest_tmp).map_err(|error| BackupFailure::Failed {
            code: "backup.manifest-write",
            message: format!("manifest 写入失败：{error}"),
        })?;
        file.write_all(&bytes)
            .map_err(|error| BackupFailure::Failed {
                code: "backup.manifest-write",
                message: format!("manifest 写入失败：{error}"),
            })?;
        file.sync_all().map_err(|error| BackupFailure::Failed {
            code: "backup.manifest-write",
            message: format!("manifest 落盘失败：{error}"),
        })?;
    }
    set_owner_only_permissions(&manifest_tmp)?;
    let manifest_path = output.join("manifest.json");
    fs::rename(&manifest_tmp, &manifest_path).map_err(|error| BackupFailure::Failed {
        code: "backup.manifest-write",
        message: format!("manifest 落盘失败：{error}"),
    })?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Verify
// ---------------------------------------------------------------------------

/// Verifies one whole backup directory: manifest shape, per-file digest and
/// byte count, integrity, schema-version consistency, secret scan, and the
/// Device credential redaction contract.
fn verify_backup_directory(from: &Path) -> Result<BackupManifest, BackupFailure> {
    let manifest_path = from.join("manifest.json");
    let manifest_bytes = fs::read(&manifest_path).map_err(|_| BackupFailure::Refused {
        code: "backup.manifest-missing",
        message: format!("备份目录缺少 manifest.json：{}", from.display()),
    })?;
    let manifest: BackupManifest =
        serde_json::from_slice(&manifest_bytes).map_err(|error| BackupFailure::Refused {
            code: "backup.manifest-invalid",
            message: format!("manifest.json 无法解析：{error}"),
        })?;
    if manifest.format != MANIFEST_FORMAT {
        return Err(BackupFailure::Refused {
            code: "backup.manifest-version",
            message: format!(
                "不支持的备份 manifest 版本：{}（仅支持 {MANIFEST_FORMAT}）。",
                manifest.format
            ),
        });
    }
    let mut findings = Vec::new();
    for entry in &manifest.databases {
        verify_backup_entry(from, entry, &mut findings);
    }
    if findings.is_empty() {
        Ok(manifest)
    } else {
        Err(BackupFailure::VerifyFailed { findings })
    }
}

fn verify_backup_entry(from: &Path, entry: &BackupDatabaseEntry, findings: &mut Vec<String>) {
    let label = entry.name.clone();
    if entry.file != format!("{}.sqlite3", entry.name) {
        findings.push(format!(
            "{label}: manifest 文件名非规范（{}）。",
            entry.file
        ));
        return;
    }
    let path = from.join(&entry.file);
    let Ok(bytes) = fs::read(&path) else {
        findings.push(format!("{label}: 快照文件缺失（{}）。", path.display()));
        return;
    };
    if bytes.len() as u64 != entry.byte_count {
        findings.push(format!(
            "{label}: 快照字节数与 manifest 不一致（{} != {}）。",
            bytes.len(),
            entry.byte_count
        ));
        return;
    }
    if sha256_digest(&bytes) != entry.sha256 {
        findings.push(format!(
            "{label}: 快照 digest 与 manifest 不一致，疑似篡改。"
        ));
        return;
    }
    if let Some(marker) = scan_secret_markers(&bytes) {
        findings.push(format!("{label}: 快照包含明文凭据标记（{marker}）。"));
        return;
    }
    let Ok(connection) = open_snapshot_connection(&path) else {
        findings.push(format!("{label}: 快照无法以只读方式打开。"));
        return;
    };
    let check: Result<String, _> = connection.query_row("PRAGMA quick_check", [], |row| row.get(0));
    match check {
        Ok(check) if check == "ok" => {}
        Ok(check) => {
            findings.push(format!("{label}: 快照完整性检查未通过（{check}）。"));
            return;
        }
        Err(error) => {
            findings.push(format!("{label}: 快照完整性检查失败（{error}）。"));
            return;
        }
    }
    let version: Result<i64, _> = connection.query_row("PRAGMA user_version", [], |row| row.get(0));
    match version {
        Ok(version) if version == entry.schema_version => {}
        Ok(version) => {
            findings.push(format!(
                "{label}: 快照 schema 版本 {version} 与 manifest 记录 {} 不一致。",
                entry.schema_version
            ));
            return;
        }
        Err(error) => {
            findings.push(format!("{label}: 读取快照 schema 版本失败（{error}）。"));
            return;
        }
    }
    if entry.name == DEVICE_CLIENT_DATABASE {
        match credential_redaction_finding(&connection) {
            Ok(()) => {}
            Err(finding) => findings.push(finding),
        }
    }
}

/// Returns `Err` when any Device snapshot credential row still carries secret
/// material instead of the 32-byte all-zero redaction form.
fn credential_redaction_finding(connection: &Connection) -> Result<(), String> {
    let rows: Vec<(String, Vec<u8>)> = {
        let mut statement = connection
            .prepare("SELECT device_id, credential_secret FROM device_credential")
            .map_err(|error| format!("device-client: device_credential 读取失败：{error}"))?;
        let mapped = statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .map_err(|error| format!("device-client: device_credential 读取失败：{error}"))?;
        mapped
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| format!("device-client: device_credential 读取失败：{error}"))?
    };
    for (device_id, secret) in rows {
        if secret.len() != 32 || secret.iter().any(|byte| *byte != 0) {
            return Err(format!(
                "device-client: 快照携带明文设备凭据（device_id {device_id}），备份无效。"
            ));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Restore
// ---------------------------------------------------------------------------

/// Restores one verified backup directory into the data directory.
///
/// Order is fail-closed by construction: verification first, then the schema
/// version gates, then the Device credential continuity check, and only then
/// per-database placement through an atomic rename.
fn restore_store(
    store: BackupStoreKind,
    data_directory: &Path,
    from: &Path,
) -> Result<Vec<String>, BackupFailure> {
    let manifest = verify_backup_directory(from)?;
    if manifest.store != store.as_str() {
        return Err(BackupFailure::Refused {
            code: "restore.store-mismatch",
            message: format!(
                "备份属于 {} store，不能恢复到 {} store。",
                manifest.store,
                store.as_str()
            ),
        });
    }
    for entry in &manifest.databases {
        if schema_gate_label(&entry.name).is_some() {
            let supported = supported_restore_versions(&entry.name).unwrap_or_default();
            if !supported.contains(&entry.schema_version) {
                return Err(BackupFailure::Refused {
                    code: "restore.unsupported-schema-version",
                    message: format!(
                        "快照 schema 版本 {} 不在可恢复集合 {supported:?} 内；\
                         恢复已拒绝，现有数据未改动（对齐启动时拒绝更新版本库的语义）。",
                        entry.schema_version
                    ),
                });
            }
        }
    }
    fs::create_dir_all(data_directory).map_err(|error| BackupFailure::Failed {
        code: "restore.data-directory",
        message: format!("数据目录不可用：{error}"),
    })?;
    let live_credentials = if store == BackupStoreKind::Device {
        Some(read_live_credentials(data_directory)?)
    } else {
        None
    };
    let mut restored = Vec::new();
    for entry in &manifest.databases {
        place_snapshot(
            store,
            data_directory,
            from,
            entry,
            live_credentials.as_ref(),
        )?;
        restored.push(entry.name.clone());
    }
    Ok(restored)
}

/// The live Device credential facts restore re-binds into the restored store.
struct LiveCredential {
    device_id: String,
    secret: Vec<u8>,
    digest: String,
}

/// Reads the live Device credential rows and proves each secret still matches
/// its stored digest, so the re-bind continues the exact live credential.
fn read_live_credentials(data_directory: &Path) -> Result<Vec<LiveCredential>, BackupFailure> {
    let live_path = data_directory.join(format!("{DEVICE_CLIENT_DATABASE}.sqlite3"));
    if !live_path.is_file() {
        return Err(BackupFailure::Refused {
            code: "restore.device-credential-unavailable",
            message: format!(
                "目标目录没有活的 Device Client 本地库（{}）；备份不含凭据，\
                 无法在新设备上恢复身份，请先完成该设备的本地初始化或重新 enrollment。",
                live_path.display()
            ),
        });
    }
    let connection = open_live_connection(&live_path)?;
    let rows: Vec<(String, Vec<u8>, String)> = {
        let mut statement = connection
            .prepare(
                "SELECT device_id, credential_secret, credential_digest \
                 FROM device_credential",
            )
            .map_err(|error| credential_read_failure(&error.to_string()))?;
        let mapped = statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .map_err(|error| credential_read_failure(&error.to_string()))?;
        mapped
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| credential_read_failure(&error.to_string()))?
    };
    if rows.is_empty() {
        return Err(BackupFailure::Refused {
            code: "restore.device-credential-unavailable",
            message: "本机 Device Client 本地库没有设备凭据行；无法完成凭据回绑。".to_owned(),
        });
    }
    let mut credentials = Vec::new();
    for (device_id, secret, digest) in rows {
        if credential_digest(&secret) != digest {
            return Err(BackupFailure::Refused {
                code: "restore.credential-mismatch",
                message: format!(
                    "本机凭据与其 digest 不一致（device_id {device_id}）：本地库异常，拒绝恢复。"
                ),
            });
        }
        credentials.push(LiveCredential {
            device_id,
            secret,
            digest,
        });
    }
    Ok(credentials)
}

fn credential_read_failure(detail: &str) -> BackupFailure {
    BackupFailure::Failed {
        code: "restore.credential-read",
        message: format!("读取本机设备凭据失败：{detail}"),
    }
}

/// Places one snapshot into the data directory: copy to a temp file (the
/// source here is a sealed snapshot file, never a live database), re-bind the
/// live Device credential, drop the replaced database's stale WAL sidecars,
/// then rename atomically.
fn place_snapshot(
    store: BackupStoreKind,
    data_directory: &Path,
    from: &Path,
    entry: &BackupDatabaseEntry,
    live_credentials: Option<&Vec<LiveCredential>>,
) -> Result<(), BackupFailure> {
    let snapshot_path = from.join(&entry.file);
    let target = data_directory.join(&entry.file);
    let restore_tmp = target_with_suffix(&target, RESTORE_TMP_SUFFIX);
    fs::copy(&snapshot_path, &restore_tmp).map_err(|error| BackupFailure::Failed {
        code: "restore.copy",
        message: format!("{} 恢复拷贝失败：{error}", entry.name),
    })?;
    set_owner_only_permissions(&restore_tmp)?;
    if store == BackupStoreKind::Device && entry.name == DEVICE_CLIENT_DATABASE {
        rebind_device_credentials(&restore_tmp, live_credentials)?;
    }
    for suffix in ["-wal", "-shm"] {
        let sidecar = target_with_suffix(&target, suffix);
        if sidecar.exists() {
            fs::remove_file(&sidecar).map_err(|error| BackupFailure::Failed {
                code: "restore.wal-sidecar",
                message: format!("移除 {} 的 {suffix} 旧文件失败：{error}", entry.name),
            })?;
        }
    }
    fs::rename(&restore_tmp, &target).map_err(|error| BackupFailure::Failed {
        code: "restore.place",
        message: format!("{} 恢复落盘失败：{error}", entry.name),
    })?;
    Ok(())
}

fn target_with_suffix(target: &Path, suffix: &str) -> PathBuf {
    let mut os_string = target.as_os_str().to_owned();
    os_string.push(suffix);
    PathBuf::from(os_string)
}

/// Re-binds the live credential secrets into the restored Device store copy
/// before it is renamed into place, so placement stays atomic and the placed
/// store is immediately usable by the live device.
fn rebind_device_credentials(
    restore_tmp: &Path,
    live_credentials: Option<&Vec<LiveCredential>>,
) -> Result<(), BackupFailure> {
    let Some(live_credentials) = live_credentials else {
        return Err(BackupFailure::Failed {
            code: "restore.credential-missing",
            message: "内部错误：Device 恢复缺少本机凭据。".to_owned(),
        });
    };
    let connection = open_live_connection(restore_tmp)?;
    let snapshot_credentials: Vec<(String, String)> = {
        let mut statement = connection
            .prepare("SELECT device_id, credential_digest FROM device_credential")
            .map_err(|error| rebind_failure(&error.to_string()))?;
        let mapped = statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .map_err(|error| rebind_failure(&error.to_string()))?;
        mapped
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| rebind_failure(&error.to_string()))?
    };
    for (device_id, snapshot_digest) in snapshot_credentials {
        let Some(live) = live_credentials
            .iter()
            .find(|credential| credential.device_id == device_id)
        else {
            return Err(BackupFailure::Refused {
                code: "restore.credential-mismatch",
                message: format!(
                    "快照中的设备 {device_id} 不在本机活库中：备份不含凭据，跨设备恢复被拒绝。"
                ),
            });
        };
        if live.digest != snapshot_digest {
            return Err(BackupFailure::Refused {
                code: "restore.credential-mismatch",
                message: format!(
                    "设备 {device_id} 的凭据在备份后已轮换（digest 不一致）：\
                     拒绝用旧快照覆盖，请先重新备份或经重新 enrollment 换身份。"
                ),
            });
        }
        connection
            .execute(
                "UPDATE device_credential SET credential_secret = ?1 WHERE device_id = ?2",
                params![live.secret, live.device_id],
            )
            .map_err(|error| rebind_failure(&error.to_string()))?;
    }
    let still_zeroed: i64 = connection
        .query_row(
            "SELECT count(*) FROM device_credential WHERE credential_secret = zeroblob(32)",
            [],
            |row| row.get(0),
        )
        .map_err(|error| rebind_failure(&error.to_string()))?;
    if still_zeroed > 0 {
        return Err(BackupFailure::Failed {
            code: "restore.credential-bind",
            message: "凭据回绑不完整：仍有清零凭据残留。".to_owned(),
        });
    }
    connection
        .close()
        .map_err(|(_, error)| rebind_failure(&error.to_string()))?;
    Ok(())
}

fn rebind_failure(detail: &str) -> BackupFailure {
    BackupFailure::Failed {
        code: "restore.credential-bind",
        message: format!("凭据回绑失败：{detail}"),
    }
}

// ---------------------------------------------------------------------------
// Repair
// ---------------------------------------------------------------------------

fn repair_store(
    store: BackupStoreKind,
    data_directory: &Path,
    apply: bool,
) -> Result<RepairReport, BackupFailure> {
    let databases = discover_databases(store, data_directory)?;
    let mut findings = Vec::new();
    for (name, path) in &databases {
        diagnose_database(name, path, &mut findings);
    }
    let mut actions = Vec::new();
    if apply {
        for (name, path) in &databases {
            checkpoint_database(name, path, &mut actions);
        }
        cleanup_stale_temp_files(data_directory, &mut actions);
    }
    let report = RepairReport {
        healthy: findings.is_empty(),
        findings,
        actions,
    };
    append_repair_log(
        data_directory,
        &RepairLogEntry {
            timestamp_millis: now_millis()?,
            store: store.as_str().to_owned(),
            mode: if apply {
                "apply".to_owned()
            } else {
                "diagnose".to_owned()
            },
            findings: report.findings.clone(),
            actions: report.actions.clone(),
        },
    )?;
    Ok(report)
}

/// Read-only diagnosis of one database. Never rewrites anything: damage
/// beyond bounded repair is reported with restore guidance.
fn diagnose_database(name: &str, path: &Path, findings: &mut Vec<RepairFinding>) {
    let connection = match open_live_connection(path) {
        Ok(connection) => connection,
        Err(failure) => {
            findings.push(RepairFinding {
                database: name.to_owned(),
                code: "open-failed".to_owned(),
                detail: failure.to_string(),
            });
            return;
        }
    };
    let integrity: Vec<String> = {
        let mut statement = match connection.prepare("PRAGMA integrity_check") {
            Ok(statement) => statement,
            Err(error) => {
                findings.push(RepairFinding {
                    database: name.to_owned(),
                    code: "integrity".to_owned(),
                    detail: format!("integrity_check 无法执行：{error}"),
                });
                return;
            }
        };
        let mapped = statement.query_map([], |row| row.get::<_, String>(0));
        match mapped {
            Ok(rows) => rows
                .collect::<Result<Vec<_>, _>>()
                .unwrap_or_else(|error| vec![format!("integrity_check 失败：{error}")]),
            Err(error) => {
                findings.push(RepairFinding {
                    database: name.to_owned(),
                    code: "integrity".to_owned(),
                    detail: format!("integrity_check 失败：{error}"),
                });
                return;
            }
        }
    };
    if integrity.len() != 1 || integrity.iter().any(|row| row != "ok") {
        for row in integrity.iter().take(MAX_FINDINGS_PER_DATABASE) {
            findings.push(RepairFinding {
                database: name.to_owned(),
                code: "integrity".to_owned(),
                detail: format!("损坏（{row}）；有界修复不改写数据，请从备份恢复。"),
            });
        }
    }
    let version: i64 = connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap_or(0);
    if let Some(label) = schema_gate_label(name) {
        let supported = supported_restore_versions(name).unwrap_or_default();
        if !supported.contains(&version) {
            findings.push(RepairFinding {
                database: name.to_owned(),
                code: "schema-version".to_owned(),
                detail: format!(
                    "{label} schema 版本 {version} 不在支持集合 {supported:?} 内；\
                     该库不能作为备份源或恢复目标。"
                ),
            });
        }
    }
    let wal = target_with_suffix(path, "-wal");
    if let Ok(metadata) = fs::metadata(&wal)
        && metadata.len() > 0
    {
        findings.push(RepairFinding {
            database: name.to_owned(),
            code: "wal-size".to_owned(),
            detail: format!("WAL 文件未合并：{} 字节。", metadata.len()),
        });
    }
}

/// Bounded action: one WAL checkpoint per database, never a rewrite.
fn checkpoint_database(name: &str, path: &Path, actions: &mut Vec<RepairAction>) {
    let outcome = checkpoint_once(path);
    match outcome {
        Ok(detail) => actions.push(RepairAction {
            action: "wal-checkpoint".to_owned(),
            target: name.to_owned(),
            outcome: "ok".to_owned(),
            detail,
        }),
        Err(failure) => actions.push(RepairAction {
            action: "wal-checkpoint".to_owned(),
            target: name.to_owned(),
            outcome: "failed".to_owned(),
            detail: failure.to_string(),
        }),
    }
}

fn checkpoint_once(path: &Path) -> Result<String, BackupFailure> {
    let connection = open_live_connection(path)?;
    let (busy, log_frames, checkpointed_frames): (i64, i64, i64) = connection
        .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })
        .map_err(|error| BackupFailure::Failed {
            code: "repair.checkpoint",
            message: format!("WAL checkpoint 失败：{error}"),
        })?;
    drop(connection);
    Ok(format!(
        "busy={busy}，合并 {log_frames} 帧，截断后余 {checkpointed_frames} 帧"
    ))
}

/// Bounded action: remove only this module's own stale temp files.
fn cleanup_stale_temp_files(data_directory: &Path, actions: &mut Vec<RepairAction>) {
    let Ok(read) = fs::read_dir(data_directory) else {
        return;
    };
    for entry in read.flatten() {
        let file_name = entry.file_name();
        let Some(name) = file_name.to_str() else {
            continue;
        };
        if !(name.ends_with(VACUUM_TMP_SUFFIX) || name.ends_with(RESTORE_TMP_SUFFIX)) {
            continue;
        }
        match fs::remove_file(entry.path()) {
            Ok(()) => actions.push(RepairAction {
                action: "stale-temp-cleanup".to_owned(),
                target: name.to_owned(),
                outcome: "ok".to_owned(),
                detail: "已移除中断运行遗留的临时文件。".to_owned(),
            }),
            Err(error) => actions.push(RepairAction {
                action: "stale-temp-cleanup".to_owned(),
                target: name.to_owned(),
                outcome: "failed".to_owned(),
                detail: format!("移除失败：{error}"),
            }),
        }
    }
}

fn append_repair_log(data_directory: &Path, entry: &RepairLogEntry) -> Result<(), BackupFailure> {
    let line = serde_json::to_string(entry).map_err(|error| BackupFailure::Failed {
        code: "repair.log-encode",
        message: format!("repair 留痕编码失败：{error}"),
    })?;
    let path = data_directory.join(REPAIR_LOG_FILE);
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|error| BackupFailure::Failed {
            code: "repair.log-write",
            message: format!("repair 留痕写入失败：{error}"),
        })?;
    set_owner_only_permissions(&path)?;
    writeln!(file, "{line}").map_err(|error| BackupFailure::Failed {
        code: "repair.log-write",
        message: format!("repair 留痕写入失败：{error}"),
    })?;
    file.sync_all().map_err(|error| BackupFailure::Failed {
        code: "repair.log-write",
        message: format!("repair 留痕落盘失败：{error}"),
    })?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Opens a live (possibly WAL) database without creating it. Diagnosis and
/// snapshot paths never issue DML on this connection.
fn open_live_connection(path: &Path) -> Result<Connection, BackupFailure> {
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_FULL_MUTEX,
    )
    .map_err(|error| BackupFailure::Failed {
        code: "backup.open",
        message: format!("{} 无法打开：{error}", path.display()),
    })?;
    connection
        .busy_timeout(BUSY_TIMEOUT)
        .map_err(|error| BackupFailure::Failed {
            code: "backup.open",
            message: format!("设置 busy timeout 失败：{error}"),
        })?;
    Ok(connection)
}

/// Opens a sealed snapshot file read-only.
fn open_snapshot_connection(path: &Path) -> Result<Connection, BackupFailure> {
    let connection =
        Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(|error| {
            BackupFailure::Failed {
                code: "backup.snapshot-open",
                message: format!("{} 无法打开：{error}", path.display()),
            }
        })?;
    connection
        .busy_timeout(BUSY_TIMEOUT)
        .map_err(|error| BackupFailure::Failed {
            code: "backup.snapshot-open",
            message: format!("设置 busy timeout 失败：{error}"),
        })?;
    Ok(connection)
}

fn set_owner_only_permissions(path: &Path) -> Result<(), BackupFailure> {
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(|error| {
        BackupFailure::Failed {
            code: "backup.permissions",
            message: format!("{} 权限设置失败：{error}", path.display()),
        }
    })
}

/// Directories need the search bit (0o700), files stay owner-only (0o600).
fn set_directory_permissions(path: &Path) -> Result<(), BackupFailure> {
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(|error| {
        BackupFailure::Failed {
            code: "backup.permissions",
            message: format!("{} 权限设置失败：{error}", path.display()),
        }
    })
}

/// Resolves one path through existing ancestors so prefix comparisons work
/// across symlinked roots (macOS `/var` -> `/private/var`).
fn canonical_existing_or_parent(path: &Path) -> Option<PathBuf> {
    if let Ok(absolute) = fs::canonicalize(path) {
        return Some(absolute);
    }
    let parent = path.parent()?;
    let canonical_parent = fs::canonicalize(parent).ok()?;
    match path.file_name() {
        Some(name) => Some(canonical_parent.join(name)),
        None => Some(canonical_parent),
    }
}

fn sha256_digest(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

/// The device credential digest formula from the Device Client identity
/// module, kept byte-identical so restore can verify the live secret against
/// snapshot metadata.
fn credential_digest(secret: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(secret))
}

/// Marker scan over raw bytes, aligned with the evidence-export secret
/// scanner. Returns the first matching marker.
fn scan_secret_markers(bytes: &[u8]) -> Option<&'static str> {
    let mut normalized = Vec::with_capacity(bytes.len());
    normalized.extend(bytes.iter().filter_map(|byte| match byte {
        b' ' | b'\t' => None,
        _ => Some(byte.to_ascii_lowercase()),
    }));
    SECRET_MARKERS
        .iter()
        .find(|marker| contains_bytes(&normalized, marker.as_bytes()))
        .copied()
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

fn now_millis() -> Result<u64, BackupFailure> {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| BackupFailure::Failed {
            code: "backup.clock",
            message: format!("系统时钟不可用：{error}"),
        })?;
    u64::try_from(elapsed.as_millis()).map_err(|error| BackupFailure::Failed {
        code: "backup.clock",
        message: format!("系统时钟超出可表示范围：{error}"),
    })
}

fn render_plain(outcome: &BackupOutcome) -> String {
    let mut output = String::new();
    match outcome {
        BackupOutcome::SnapshotComplete {
            store,
            backup_directory,
            databases,
        } => {
            output.push_str("备份完成。\n");
            let _ = writeln!(output, "Store：{store}\n备份目录：{backup_directory}");
            for entry in databases {
                let _ = writeln!(
                    output,
                    "- {}：schema 版本 {}，{} 字节，sha256 {}（凭据已清零：{}）",
                    entry.name,
                    entry.schema_version,
                    entry.byte_count,
                    entry.sha256,
                    if entry.credential_redacted {
                        "是"
                    } else {
                        "否"
                    }
                );
            }
            output.push_str(
                "说明：快照经 VACUUM INTO 一致性 cut，产物不含明文凭据；可随时用 wwc backup verify 校验。\n",
            );
        }
        BackupOutcome::VerifyOk { store, databases } => {
            output.push_str("备份校验通过。\n");
            let _ = writeln!(output, "Store：{store}");
            for entry in databases {
                let _ = writeln!(
                    output,
                    "- {}：schema 版本 {}，{} 字节，digest 与完整性均一致",
                    entry.name, entry.schema_version, entry.byte_count
                );
            }
            output.push_str("说明：未发现明文凭据，未发现篡改。\n");
        }
        BackupOutcome::RestoreComplete {
            store,
            databases,
            note,
        } => {
            output.push_str("恢复完成。\n");
            let _ = writeln!(output, "Store：{store}\n已恢复：{}", databases.join("、"));
            let _ = writeln!(output, "{note}");
        }
        BackupOutcome::RepairReport {
            mode,
            healthy,
            findings,
            actions,
            log_file,
        } => {
            output.push_str(if *healthy {
                "repair 诊断：未发现损坏。\n"
            } else {
                "repair 诊断：发现需要处理的问题（不改写数据，请从备份恢复）。\n"
            });
            let _ = writeln!(output, "模式：{mode}");
            for finding in findings {
                let _ = writeln!(
                    output,
                    "- [{}] {}：{}",
                    finding.code, finding.database, finding.detail
                );
            }
            for action in actions {
                let _ = writeln!(
                    output,
                    "- 动作 {}（{}）：{} — {}",
                    action.action, action.target, action.outcome, action.detail
                );
            }
            let _ = writeln!(output, "留痕：{log_file}");
        }
    }
    output
}

fn render_failure(failure: &BackupFailure) -> (i32, String) {
    match failure {
        BackupFailure::Usage { .. } => (EXIT_USAGE, failure.to_string()),
        BackupFailure::NotInitialized { .. } => (EXIT_ACTION_REQUIRED, failure.to_string()),
        BackupFailure::VerifyFailed { .. } => (EXIT_DIAGNOSTIC_FAILED, failure.to_string()),
        BackupFailure::Refused { code, .. } | BackupFailure::Failed { code, .. } => {
            (EXIT_SERVICE, format!("[{code}] {failure}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    use winwincode_device_client::{
        DeviceIdentitySeed, DeviceStore, ensure_device_identity, load_device_identity,
    };
    use winwincode_domain::UserAccountRole;
    use winwincode_server::UserAccountService;

    use crate::cli::WwcCliExit;
    use crate::user_admin::{UserAccountAdmin, UserAdminOutcome};

    static SEQUENCE: AtomicU64 = AtomicU64::new(0);

    struct Fixture {
        root: PathBuf,
    }

    impl Fixture {
        fn new(label: &str) -> Self {
            let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let root = std::env::temp_dir().join(format!(
                "winwincode-cli-backup-{label}-{}-{sequence}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&root);
            fs::create_dir_all(&root).expect("fixture root");
            Self { root }
        }

        fn data_dir(&self) -> PathBuf {
            self.root.join("data")
        }

        fn backup_dir(&self) -> PathBuf {
            self.root.join("backup")
        }

        fn data_arg(&self) -> String {
            self.data_dir().display().to_string()
        }

        fn backup_arg(&self) -> String {
            self.backup_dir().display().to_string()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn run(arguments: &[&str]) -> WwcCliExit {
        let owned: Vec<String> = arguments.iter().map(|value| (*value).to_owned()).collect();
        run_backup(&owned)
    }

    fn exit_json(exit: &WwcCliExit) -> serde_json::Value {
        serde_json::from_str(&exit.stdout).expect("JSON stdout")
    }

    fn assert_success(exit: &WwcCliExit) -> serde_json::Value {
        assert_eq!(exit.code, EXIT_SUCCESS, "stderr: {}", exit.stderr);
        if exit.stdout.trim_start().starts_with('{') {
            exit_json(exit)
        } else {
            serde_json::Value::Null
        }
    }

    fn connection(path: &Path) -> Connection {
        Connection::open(path).expect("raw test connection")
    }

    fn seed_server_store(data_directory: &Path) -> (String, String, String) {
        let admin = UserAccountAdmin::open(data_directory.to_path_buf());
        let outcome = admin
            .create("ops-owner", UserAccountRole::Owner)
            .expect("owner account");
        let (user_id, username, temporary_password) = match outcome {
            UserAdminOutcome::UserCreated {
                user,
                temporary_password,
            } => (
                user.user_id.clone(),
                user.username.clone(),
                temporary_password,
            ),
            other => panic!("unexpected admin outcome: {other:?}"),
        };
        let database = connection(&data_directory.join("control-plane.sqlite3"));
        database
            .execute_batch(
                "INSERT INTO command_receipts \
                 (actor_key, scope_key, request_id, command_digest, stream_id, revision) \
                 VALUES (X'A5A5A5A5', X'5A5A5A5A', 'req-ops1005-cursor', \
                 'sha256:test-digest', 'stream-ops1005', 3);
                 INSERT INTO outbox \
                 (event_id, receipt_actor_key, receipt_scope_key, request_id, topic, \
                  payload, published) \
                 VALUES ('evt-ops1005-1', X'A5A5A5A5', X'5A5A5A5A', 'req-ops1005-cursor', \
                 'ops-100-5-test', X'01020304', 0);
                 INSERT INTO projection_event_stream_heads \
                 (scope_key, stream_kind, resource_id, sequence, event_id) \
                 VALUES (X'5A5A5A5A', 'scope', 'resource-ops1005', 7, 'evt-ops1005-1');",
            )
            .expect("seed receipts, outbox, and cursor");
        drop(database);
        // A sidecar the Server owns next to the product database: it stores
        // only session digests and does not manage user_version.
        let sidecar = connection(&data_directory.join("auth-sessions.sqlite3"));
        sidecar
            .execute_batch(
                "CREATE TABLE auth_sessions (
                     session_digest TEXT PRIMARY KEY NOT NULL CHECK(length(session_digest) = 64),
                     subject TEXT NOT NULL,
                     actor_json TEXT NOT NULL,
                     authorized_scopes_json TEXT NOT NULL,
                     created_at_millis INTEGER NOT NULL,
                     expires_at_millis INTEGER NOT NULL,
                     revoked_at_millis INTEGER
                 );
                 INSERT INTO auth_sessions VALUES
                 ('aa71ae5f2b6f4d9c1a2b3c4d5e6f708192a3b4c5d6e7f8091a2b3c4d5e6faa71', \
                  'usr_owner', '{\"kind\":\"user\"}', '[\"baseline\"]', 1, 2, NULL);",
            )
            .expect("seed auth session sidecar");
        drop(sidecar);
        (user_id, username, temporary_password)
    }

    fn count(data_directory: &Path, file: &str, sql: &str) -> i64 {
        let database = connection(&data_directory.join(file));
        let value: i64 = database
            .query_row(sql, [], |row| row.get(0))
            .expect("count query");
        value
    }

    fn patch_snapshot_version(backup_directory: &Path, name: &str, new_version: i64) {
        let snapshot_path = backup_directory.join(format!("{name}.sqlite3"));
        {
            let database = connection(&snapshot_path);
            database
                .pragma_update(None, "user_version", new_version)
                .expect("patch user_version");
        }
        let manifest_path = backup_directory.join("manifest.json");
        let bytes = fs::read(&manifest_path).expect("manifest");
        let mut manifest: BackupManifest = serde_json::from_slice(&bytes).expect("manifest json");
        for entry in &mut manifest.databases {
            if entry.name == name {
                entry.schema_version = new_version;
                let snapshot_bytes = fs::read(&snapshot_path).expect("snapshot");
                entry.byte_count = snapshot_bytes.len() as u64;
                entry.sha256 = sha256_digest(&snapshot_bytes);
            }
        }
        fs::write(
            &manifest_path,
            serde_json::to_vec_pretty(&manifest).expect("manifest encode"),
        )
        .expect("rewrite manifest");
    }

    fn device_seed() -> DeviceIdentitySeed {
        DeviceIdentitySeed {
            display_name: "ops-backup-test".to_owned(),
            platform: "darwin".to_owned(),
            architecture: "arm64".to_owned(),
            client_version: "0.0.0-test".to_owned(),
        }
    }

    /// Creates one initialized Device store and returns (`device_id`, `secret`).
    fn seed_device_store(data_directory: &Path) -> (String, Vec<u8>) {
        let mut store = DeviceStore::open(data_directory).expect("device store");
        let record = ensure_device_identity(&mut store, &device_seed(), "2026-09-05T00:00:00Z")
            .expect("device identity");
        let device_id = record.identity().device_id().to_owned();
        let secret = record.credential().expose_secret().to_vec();
        drop(store);
        let database = connection(&data_directory.join("device-client.sqlite3"));
        database
            .execute_batch(
                "INSERT INTO client_outbox \
                 (message_id, client_node_id, client_instance_id, envelope_sequence, kind, \
                  payload, occurred_at, published) \
                 VALUES ('msg-ops1005-1', 'cnd_test', 'cix_test', 1, \
                 'repository.registered', X'0102', '2026-09-05T00:00:00Z', 0);
                 INSERT INTO client_inbox_cursor \
                 (server_profile_id, last_sequence, last_message_id, updated_at) \
                 VALUES ('srvp_test', 41, 'srv-msg-41', '2026-09-05T00:00:00Z');",
            )
            .expect("seed outbox and cursor");
        drop(database);
        (device_id, secret)
    }

    fn secret_in_file(secret: &[u8], path: &Path) -> bool {
        let bytes = fs::read(path).expect("snapshot bytes");
        secret.len() <= bytes.len() && bytes.windows(secret.len()).any(|window| window == secret)
    }

    #[test]
    fn snapshot_and_restore_round_trip_server_ids_cursors_and_receipts() {
        let fixture = Fixture::new("server-round-trip");
        let data = fixture.data_arg();
        let backup = fixture.backup_arg();
        let (user_id, username, temporary_password) = seed_server_store(&fixture.data_dir());
        let data_directory = fixture.data_dir();

        let snapshot = run(&[
            "snapshot",
            "--store",
            "server",
            "--data-dir",
            &data,
            "--output",
            &backup,
            "--json",
        ]);
        let value = assert_success(&snapshot);
        assert_eq!(value["status"], "snapshot-complete");
        let names: Vec<&str> = value["databases"]
            .as_array()
            .expect("databases")
            .iter()
            .map(|entry| entry["name"].as_str().expect("name"))
            .collect();
        assert_eq!(names, vec!["auth-sessions", "control-plane"]);

        // Simulate loss: wipe the live facts after the backup exists.
        {
            let database = connection(&data_directory.join("control-plane.sqlite3"));
            database
                .execute_batch(
                    "DELETE FROM projection_event_stream_heads; DELETE FROM outbox; \
                     DELETE FROM command_receipts; DELETE FROM users;",
                )
                .expect("wipe live state");
        }
        assert_eq!(
            count(
                &data_directory,
                "control-plane.sqlite3",
                "SELECT count(*) FROM users"
            ),
            0
        );

        let restore = run(&[
            "restore",
            "--store",
            "server",
            "--data-dir",
            &data,
            "--from",
            &backup,
            "--json",
        ]);
        let value = assert_success(&restore);
        assert_eq!(value["status"], "restore-complete");
        assert_server_restored(&data_directory, &user_id, &username, &temporary_password);
    }

    /// Asserts the exact owner identity, the seeded receipt, the outbox
    /// event, the projection cursor, and the auth-session digest all came
    /// back through the snapshot.
    fn assert_server_restored(
        data_directory: &Path,
        user_id: &str,
        username: &str,
        temporary_password: &str,
    ) {
        // IDs and the credential fact survive: the exact owner account opens
        // and the exact temporary password still verifies.
        let service = UserAccountService::open(data_directory).expect("reopen storage");
        let verified = service
            .verify_credentials(username, temporary_password)
            .expect("verification runs")
            .expect("credential survives restore");
        assert_eq!(verified.user_id.0, user_id);
        // Receipts survive byte-exactly: the seeded receipt is the only one
        // (account creation itself writes no command receipt).
        assert_eq!(
            count(
                data_directory,
                "control-plane.sqlite3",
                "SELECT count(*) FROM command_receipts \
                 WHERE request_id = 'req-ops1005-cursor'"
            ),
            1
        );
        assert_eq!(
            count(
                data_directory,
                "control-plane.sqlite3",
                "SELECT count(*) FROM command_receipts"
            ),
            1
        );
        let database = connection(&data_directory.join("control-plane.sqlite3"));
        let event_id: String = database
            .query_row(
                "SELECT event_id FROM outbox WHERE request_id = 'req-ops1005-cursor'",
                [],
                |row| row.get(0),
            )
            .expect("outbox row");
        assert_eq!(event_id, "evt-ops1005-1");
        let cursor: i64 = database
            .query_row(
                "SELECT sequence FROM projection_event_stream_heads \
                 WHERE resource_id = 'resource-ops1005'",
                [],
                |row| row.get(0),
            )
            .expect("cursor row");
        assert_eq!(cursor, 7);
        let sidecar = connection(&data_directory.join("auth-sessions.sqlite3"));
        let digest: String = sidecar
            .query_row("SELECT session_digest FROM auth_sessions", [], |row| {
                row.get(0)
            })
            .expect("session row");
        assert_eq!(digest.len(), 64);
    }

    #[test]
    fn restore_fails_closed_on_newer_control_plane_schema_version() {
        let fixture = Fixture::new("server-newer-schema");
        let data = fixture.data_arg();
        let backup = fixture.backup_arg();
        let (user_id, username, temporary_password) = seed_server_store(&fixture.data_dir());
        let data_directory = fixture.data_dir();
        assert_success(&run(&[
            "snapshot",
            "--store",
            "server",
            "--data-dir",
            &data,
            "--output",
            &backup,
        ]));
        patch_snapshot_version(
            &fixture.backup_dir(),
            "control-plane",
            SERVER_CONTROL_PLANE_SCHEMA_VERSION + 1,
        );

        let restore = run(&[
            "restore",
            "--store",
            "server",
            "--data-dir",
            &data,
            "--from",
            &backup,
        ]);
        assert_eq!(restore.code, EXIT_SERVICE, "stderr: {}", restore.stderr);
        assert!(
            restore
                .stderr
                .contains("restore.unsupported-schema-version")
        );
        assert!(restore.stderr.contains("未改动"));

        // Fail-closed means the live store is untouched.
        let service = UserAccountService::open(&data_directory).expect("reopen storage");
        let verified = service
            .verify_credentials(&username, &temporary_password)
            .expect("verification runs")
            .expect("account still live");
        assert_eq!(verified.user_id.0, user_id);
    }

    #[test]
    fn restore_fails_closed_on_unsupported_device_schema_version() {
        let fixture = Fixture::new("device-newer-schema");
        let data = fixture.data_arg();
        let backup = fixture.backup_arg();
        let (device_id, secret) = seed_device_store(&fixture.data_dir());
        let data_directory = fixture.data_dir();
        assert_success(&run(&[
            "snapshot",
            "--store",
            "device",
            "--data-dir",
            &data,
            "--output",
            &backup,
        ]));
        patch_snapshot_version(&fixture.backup_dir(), "device-client", 7);

        let restore = run(&[
            "restore",
            "--store",
            "device",
            "--data-dir",
            &data,
            "--from",
            &backup,
        ]);
        assert_eq!(restore.code, EXIT_SERVICE, "stderr: {}", restore.stderr);
        assert!(
            restore
                .stderr
                .contains("restore.unsupported-schema-version")
        );

        let store = DeviceStore::open(&data_directory).expect("live store untouched");
        let record = load_device_identity(&store)
            .expect("identity load")
            .expect("identity present");
        assert_eq!(record.identity().device_id(), device_id);
        assert_eq!(record.credential().expose_secret(), secret);
    }

    #[test]
    fn device_snapshot_redacts_credential_and_restore_keeps_identity_state() {
        let fixture = Fixture::new("device-round-trip");
        let data = fixture.data_arg();
        let backup = fixture.backup_arg();
        let (device_id, secret) = seed_device_store(&fixture.data_dir());
        let data_directory = fixture.data_dir();

        let snapshot = run(&[
            "snapshot",
            "--store",
            "device",
            "--data-dir",
            &data,
            "--output",
            &backup,
            "--json",
        ]);
        let value = assert_success(&snapshot);
        assert_eq!(value["status"], "snapshot-complete");
        let entry = &value["databases"][0];
        assert_eq!(entry["name"], "device-client");
        assert_eq!(entry["credential_redacted"], true);

        // The backup artifact carries no plaintext credential.
        let snapshot_path = fixture.backup_dir().join("device-client.sqlite3");
        assert!(!secret_in_file(&secret, &snapshot_path));
        {
            let database = connection(&snapshot_path);
            let (length, nonzero): (i64, i64) = database
                .query_row(
                    "SELECT length(credential_secret), \
                     (SELECT count(*) FROM device_credential \
                      WHERE credential_secret != zeroblob(32)) \
                     FROM device_credential",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .expect("redaction check");
            assert_eq!(length, 32);
            assert_eq!(nonzero, 0);
        }
        // The live store keeps the real secret.
        {
            let store = DeviceStore::open(&data_directory).expect("live store");
            let record = load_device_identity(&store)
                .expect("identity load")
                .expect("identity present");
            assert_eq!(record.credential().expose_secret(), secret);
        }

        // Simulate local state loss while identity and credential survive.
        {
            let database = connection(&data_directory.join("device-client.sqlite3"));
            database
                .execute_batch("DELETE FROM client_outbox; DELETE FROM client_inbox_cursor;")
                .expect("wipe live state");
        }
        let restore = run(&[
            "restore",
            "--store",
            "device",
            "--data-dir",
            &data,
            "--from",
            &backup,
            "--json",
        ]);
        let value = assert_success(&restore);
        assert_eq!(value["status"], "restore-complete");

        let store = DeviceStore::open(&data_directory).expect("reopen store");
        let record = load_device_identity(&store)
            .expect("identity load")
            .expect("identity present");
        assert_eq!(record.identity().device_id(), device_id);
        assert_eq!(record.credential().expose_secret(), secret);
        assert_eq!(
            count(
                &data_directory,
                "device-client.sqlite3",
                "SELECT count(*) FROM client_outbox WHERE message_id = 'msg-ops1005-1'"
            ),
            1
        );
        let cursor: i64 = {
            let database = connection(&data_directory.join("device-client.sqlite3"));
            database
                .query_row(
                    "SELECT last_sequence FROM client_inbox_cursor \
                     WHERE server_profile_id = 'srvp_test'",
                    [],
                    |row| row.get(0),
                )
                .expect("cursor row")
        };
        assert_eq!(cursor, 41);
    }

    #[test]
    fn restore_rejects_snapshot_when_live_device_credential_rotated() {
        let fixture = Fixture::new("device-credential-mismatch");
        let data = fixture.data_arg();
        let backup = fixture.backup_arg();
        seed_device_store(&fixture.data_dir());
        let data_directory = fixture.data_dir();
        assert_success(&run(&[
            "snapshot",
            "--store",
            "device",
            "--data-dir",
            &data,
            "--output",
            &backup,
        ]));
        {
            let database = connection(&data_directory.join("device-client.sqlite3"));
            database
                .execute("UPDATE device_credential SET credential_digest = ?", [])
                .expect_err("missing parameter should fail");
            database
                .execute(
                    "UPDATE device_credential SET credential_digest = ?1",
                    params![format!("sha256:{}", "0".repeat(64))],
                )
                .expect("rotate digest");
        }
        let restore = run(&[
            "restore",
            "--store",
            "device",
            "--data-dir",
            &data,
            "--from",
            &backup,
        ]);
        assert_eq!(restore.code, EXIT_SERVICE, "stderr: {}", restore.stderr);
        assert!(restore.stderr.contains("restore.credential-mismatch"));
    }

    #[test]
    fn restore_rejects_device_backup_without_live_store() {
        let fixture = Fixture::new("device-cross-restore");
        let backup = fixture.backup_arg();
        let device_data = fixture.root.join("origin");
        seed_device_store(&device_data);
        assert_success(&run(&[
            "snapshot",
            "--store",
            "device",
            "--data-dir",
            &device_data.display().to_string(),
            "--output",
            &backup,
        ]));
        let other = fixture.root.join("other-device");
        let restore = run(&[
            "restore",
            "--store",
            "device",
            "--data-dir",
            &other.display().to_string(),
            "--from",
            &backup,
        ]);
        assert_eq!(restore.code, EXIT_SERVICE, "stderr: {}", restore.stderr);
        assert!(
            restore
                .stderr
                .contains("restore.device-credential-unavailable")
        );
    }

    #[test]
    fn verify_rejects_snapshot_carrying_plaintext_device_credential() {
        let fixture = Fixture::new("verify-secret");
        let data = fixture.data_arg();
        let backup = fixture.backup_arg();
        let (_, secret) = seed_device_store(&fixture.data_dir());
        assert_success(&run(&[
            "snapshot",
            "--store",
            "device",
            "--data-dir",
            &data,
            "--output",
            &backup,
        ]));
        // A snapshot that carries the raw secret again must fail verification.
        {
            let snapshot_path = fixture.backup_dir().join("device-client.sqlite3");
            let database = connection(&snapshot_path);
            database
                .execute(
                    "UPDATE device_credential SET credential_secret = ?1",
                    params![secret],
                )
                .expect("re-plant secret");
            drop(database);
            // Re-seal the manifest so verification reaches the redaction
            // check instead of stopping at the digest mismatch first.
            let manifest_path = fixture.backup_dir().join("manifest.json");
            let bytes = fs::read(&manifest_path).expect("manifest");
            let mut manifest: BackupManifest =
                serde_json::from_slice(&bytes).expect("manifest json");
            let snapshot_bytes = fs::read(&snapshot_path).expect("snapshot");
            for entry in &mut manifest.databases {
                entry.byte_count = snapshot_bytes.len() as u64;
                entry.sha256 = sha256_digest(&snapshot_bytes);
            }
            fs::write(
                &manifest_path,
                serde_json::to_vec_pretty(&manifest).expect("manifest encode"),
            )
            .expect("rewrite manifest");
        }
        let verify = run(&["verify", "--from", &fixture.backup_arg()]);
        assert_eq!(
            verify.code, EXIT_DIAGNOSTIC_FAILED,
            "stderr: {}",
            verify.stderr
        );
        assert!(verify.stderr.contains("明文设备凭据"));
    }

    #[test]
    fn verify_rejects_tampered_snapshot_bytes() {
        let fixture = Fixture::new("verify-tamper");
        let data = fixture.data_arg();
        let backup = fixture.backup_arg();
        seed_device_store(&fixture.data_dir());
        assert_success(&run(&[
            "snapshot",
            "--store",
            "device",
            "--data-dir",
            &data,
            "--output",
            &backup,
        ]));
        let snapshot_path = fixture.backup_dir().join("device-client.sqlite3");
        let mut bytes = fs::read(&snapshot_path).expect("snapshot");
        bytes.push(0x41);
        fs::write(&snapshot_path, bytes).expect("tamper");
        let verify = run(&["verify", "--from", &fixture.backup_arg()]);
        assert_eq!(
            verify.code, EXIT_DIAGNOSTIC_FAILED,
            "stderr: {}",
            verify.stderr
        );
        assert!(verify.stderr.contains("字节") || verify.stderr.contains("digest"));
    }

    #[test]
    fn snapshot_refuses_output_inside_data_directory_and_requires_store() {
        let fixture = Fixture::new("usage");
        let data = fixture.data_arg();
        seed_device_store(&fixture.data_dir());
        let inside = fixture
            .data_dir()
            .join("nested-backup")
            .display()
            .to_string();
        let refused = run(&[
            "snapshot",
            "--store",
            "device",
            "--data-dir",
            &data,
            "--output",
            &inside,
        ]);
        assert_eq!(refused.code, EXIT_SERVICE);
        assert!(
            refused
                .stderr
                .contains("backup.output-inside-data-directory")
        );

        let usage = run(&[
            "snapshot",
            "--data-dir",
            &data,
            "--output",
            &fixture.backup_arg(),
        ]);
        assert_eq!(usage.code, EXIT_USAGE);
        assert!(usage.stderr.contains("--store"));

        let unknown = run(&["explode"]);
        assert_eq!(unknown.code, EXIT_USAGE);
    }

    #[test]
    fn snapshot_requires_an_initialized_store() {
        let fixture = Fixture::new("not-initialized");
        let empty = fixture.root.join("empty");
        fs::create_dir_all(&empty).expect("empty dir");
        let snapshot = run(&[
            "snapshot",
            "--store",
            "device",
            "--data-dir",
            &empty.display().to_string(),
            "--output",
            &fixture.backup_arg(),
        ]);
        assert_eq!(snapshot.code, EXIT_ACTION_REQUIRED);
        let restore = run(&[
            "restore",
            "--store",
            "device",
            "--data-dir",
            &empty.display().to_string(),
            "--from",
            &fixture.backup_arg(),
        ]);
        assert_eq!(restore.code, EXIT_SERVICE);
    }

    #[test]
    fn repair_without_apply_is_read_only_and_records_a_trace() {
        let fixture = Fixture::new("repair-diagnose");
        let data = fixture.data_arg();
        let (device_id, secret) = seed_device_store(&fixture.data_dir());
        let data_directory = fixture.data_dir();
        let before = fs::read(data_directory.join("device-client.sqlite3")).expect("before bytes");

        let repair = run(&["repair", "--store", "device", "--data-dir", &data, "--json"]);
        let value = assert_success(&repair);
        assert_eq!(value["status"], "repair-report");
        assert_eq!(value["mode"], "diagnose");
        assert_eq!(value["healthy"], true);

        let log_path = data_directory.join(REPAIR_LOG_FILE);
        let log = fs::read_to_string(&log_path).expect("repair log");
        let entry: serde_json::Value =
            serde_json::from_str(log.lines().next().expect("one log line")).expect("log line json");
        assert_eq!(entry["mode"], "diagnose");
        assert_eq!(entry["store"], "device");
        assert_eq!(entry["findings"].as_array().expect("findings").len(), 0);

        // Read-only: the database bytes did not change.
        let after = fs::read(data_directory.join("device-client.sqlite3")).expect("after bytes");
        assert_eq!(before, after);
        let store = DeviceStore::open(&data_directory).expect("store still opens");
        let record = load_device_identity(&store)
            .expect("identity load")
            .expect("identity present");
        assert_eq!(record.identity().device_id(), device_id);
        assert_eq!(record.credential().expose_secret(), secret);
    }

    #[test]
    fn repair_apply_checkpoints_wal_and_removes_stale_temp_files() {
        let fixture = Fixture::new("repair-apply");
        let data = fixture.data_arg();
        seed_device_store(&fixture.data_dir());
        let data_directory = fixture.data_dir();
        let stale = data_directory.join(format!("device-client.sqlite3{VACUUM_TMP_SUFFIX}"));
        fs::write(&stale, b"interrupted run").expect("stale temp file");

        let repair = run(&[
            "repair",
            "--store",
            "device",
            "--data-dir",
            &data,
            "--apply",
            "--json",
        ]);
        let value = assert_success(&repair);
        assert_eq!(value["healthy"], true);
        let actions = value["actions"].as_array().expect("actions");
        assert!(actions.iter().any(|action| {
            action["action"] == "stale-temp-cleanup"
                && action["target"]
                    .as_str()
                    .expect("target")
                    .ends_with(VACUUM_TMP_SUFFIX)
        }));
        assert!(
            actions
                .iter()
                .any(|action| action["action"] == "wal-checkpoint" && action["outcome"] == "ok")
        );
        assert!(!stale.exists(), "stale temp file removed");
        assert_eq!(
            count(
                &data_directory,
                "device-client.sqlite3",
                "SELECT count(*) FROM device_credential"
            ),
            1
        );
        let log = fs::read_to_string(data_directory.join(REPAIR_LOG_FILE)).expect("log");
        let entry: serde_json::Value =
            serde_json::from_str(log.lines().last().expect("line")).expect("json");
        assert_eq!(entry["mode"], "apply");
        assert_eq!(
            entry["actions"].as_array().expect("actions").len(),
            actions.len()
        );
    }

    #[test]
    fn repair_reports_corruption_and_points_to_restore() {
        let fixture = Fixture::new("repair-corrupt");
        let data = fixture.data_arg();
        seed_device_store(&fixture.data_dir());
        let data_directory = fixture.data_dir();
        let database_path = data_directory.join("device-client.sqlite3");
        {
            use std::io::{Seek as _, SeekFrom, Write as _};
            let mut file = fs::OpenOptions::new()
                .write(true)
                .open(&database_path)
                .expect("database file");
            file.seek(SeekFrom::Start(0)).expect("seek");
            file.write_all(b"CORRUPTED").expect("damage header");
        }
        let repair = run(&["repair", "--store", "device", "--data-dir", &data]);
        assert_eq!(
            repair.code, EXIT_DIAGNOSTIC_FAILED,
            "stderr: {}",
            repair.stderr
        );
        assert!(repair.stdout.contains("请从备份恢复") || repair.stderr.contains("请从备份恢复"));

        let log = fs::read_to_string(data_directory.join(REPAIR_LOG_FILE)).expect("log");
        let entry: serde_json::Value =
            serde_json::from_str(log.lines().next().expect("line")).expect("json");
        let findings = entry["findings"].as_array().expect("findings");
        assert!(!findings.is_empty(), "corruption is recorded");
    }

    #[test]
    fn secret_marker_scan_finds_plaintext_and_ignores_digests() {
        assert_eq!(
            scan_secret_markers(b"........authorization:bearer token........"),
            Some("authorization:bearer")
        );
        assert_eq!(scan_secret_markers(b"plain text without markers"), None);
        // Argon2id hashes and sha256 digests are not plaintext credentials.
        assert_eq!(
            scan_secret_markers(b"$argon2id$v=19$m=19456,t=2,p=1$c2FsdHNTYWx0c1NhbHRTYWx0$digest",),
            None
        );
        assert_eq!(
            scan_secret_markers(
                b"sha256:aa71ae5f2b6f4d9c1a2b3c4d5e6f708192a3b4c5d6e7f8091a2b3c4d5e6faa71"
            ),
            None
        );
    }

    #[test]
    fn credential_digest_matches_device_client_formula() {
        assert_eq!(
            credential_digest(b"test-secret"),
            format!("sha256:{:x}", Sha256::digest(b"test-secret"))
        );
    }

    #[test]
    fn supported_restore_versions_cover_migration_set_only() {
        let server = supported_restore_versions(CONTROL_PLANE_DATABASE).expect("gated");
        assert_eq!(
            server,
            vec![1, 2, 3, 4, 5, SERVER_CONTROL_PLANE_SCHEMA_VERSION]
        );
        let device = supported_restore_versions(DEVICE_CLIENT_DATABASE).expect("gated");
        assert_eq!(
            device,
            vec![winwincode_device_client::CLIENT_STORE_SCHEMA_VERSION]
        );
        assert_eq!(
            supported_restore_versions("auth-sessions"),
            None,
            "sidecars without a managed schema version are integrity-only"
        );
    }
}
