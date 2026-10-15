// SPDX-License-Identifier: Apache-2.0

use std::fmt;
use std::fmt::Write as _;
use std::fs;
use std::io::Write as _;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OpenFlags, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use winwincode_storage::CONTROL_PLANE_SCHEMA_VERSION as SERVER_CONTROL_PLANE_SCHEMA_VERSION;

use crate::cli::WwcCliExit;

pub const EXIT_SUCCESS: i32 = 0;
pub const EXIT_DIAGNOSTIC_FAILED: i32 = 1;
pub const EXIT_USAGE: i32 = 2;
pub const EXIT_ACTION_REQUIRED: i32 = 3;
pub const EXIT_SERVICE: i32 = 5;

const MANIFEST_FORMAT: &str = "winwincode.local-backup.v1";

const CONTROL_PLANE_DATABASE: &str = "control-plane";
const DEVICE_CLIENT_DATABASE: &str = "device-client";

const VACUUM_TMP_SUFFIX: &str = ".vacuum-tmp";
const RESTORE_TMP_SUFFIX: &str = ".restore-tmp";
const REPAIR_LOG_FILE: &str = "backup-repair-log.jsonl";

const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

const MAX_FINDINGS_PER_DATABASE: usize = 5;

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

pub const BACKUP_HELP_LINES: [&str; 7] = [
    "  wwc backup snapshot --store server|device --data-dir PATH --output PATH [--json]",
    "  wwc backup verify --from BACKUP-DIR [--json]",
    "  wwc backup restore --store server|device --data-dir PATH --from BACKUP-DIR [--json]",
    "  wwc backup repair --store server|device --data-dir PATH [--apply] [--json]",
    "backup 用 SQLite VACUUM INTO 取一致性快照，可用 wwc backup verify 校验。",
    "Device 快照保留身份和运行状态，可恢复到空数据目录。",
    "restore 校验 schema 版本 fail closed；repair 默认只读诊断，--apply 只执行白名单动作并留痕。",
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BackupStoreKind {
    Server,
    Device,
}

impl BackupStoreKind {
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

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BackupDatabaseEntry {
    pub name: String,
    pub file: String,
    pub schema_version: i64,
    pub byte_count: u64,
    pub sha256: String,
    pub integrity: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BackupManifest {
    pub format: String,
    pub store: String,
    pub created_at_millis: u64,
    pub databases: Vec<BackupDatabaseEntry>,
}

#[derive(Clone, Debug, Serialize)]
pub struct RepairLogEntry {
    pub timestamp_millis: u64,
    pub store: String,
    pub mode: String,
    pub findings: Vec<RepairFinding>,
    pub actions: Vec<RepairAction>,
}

#[derive(Clone, Debug, Serialize)]
pub struct RepairFinding {
    pub database: String,
    pub code: String,
    pub detail: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct RepairAction {
    pub action: String,
    pub target: String,
    pub outcome: String,
    pub detail: String,
}

#[derive(Debug, Serialize)]
#[serde(
    tag = "status",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase"
)]
pub enum BackupOutcome {
    SnapshotComplete {
        store: String,
        backup_directory: String,
        databases: Vec<BackupDatabaseEntry>,
    },
    VerifyOk {
        store: String,
        databases: Vec<BackupDatabaseEntry>,
    },
    RestoreComplete {
        store: String,
        databases: Vec<String>,
        note: String,
    },
    RepairReport {
        mode: String,
        healthy: bool,
        findings: Vec<RepairFinding>,
        actions: Vec<RepairAction>,
        log_file: String,
    },
}

#[derive(Clone, Debug)]
pub enum BackupFailure {
    Usage { message: String },
    NotInitialized { message: String },
    VerifyFailed { findings: Vec<String> },
    Refused { code: &'static str, message: String },
    Failed { code: &'static str, message: String },
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

fn supported_restore_versions(name: &str) -> Option<Vec<i64>> {
    match name {
        CONTROL_PLANE_DATABASE => Some(vec![SERVER_CONTROL_PLANE_SCHEMA_VERSION]),
        DEVICE_CLIENT_DATABASE => Some(vec![winwincode_device_client::CLIENT_STORE_SCHEMA_VERSION]),
        _ => None,
    }
}

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
        let entry = seal_snapshot(&vacuum_tmp, &snapshot_path, &name, schema_version)
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

fn seal_snapshot(
    vacuum_tmp: &Path,
    snapshot_path: &Path,
    name: &str,
    schema_version: i64,
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
    })
}

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
        }
        Err(error) => {
            findings.push(format!("{label}: 读取快照 schema 版本失败（{error}）。"));
        }
    }
}

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
    let mut restored = Vec::new();
    for entry in &manifest.databases {
        place_snapshot(data_directory, from, entry)?;
        restored.push(entry.name.clone());
    }
    Ok(restored)
}

fn place_snapshot(
    data_directory: &Path,
    from: &Path,
    entry: &BackupDatabaseEntry,
) -> Result<(), BackupFailure> {
    let snapshot_path = from.join(&entry.file);
    let target = data_directory.join(&entry.file);
    let restore_tmp = target_with_suffix(&target, RESTORE_TMP_SUFFIX);
    fs::copy(&snapshot_path, &restore_tmp).map_err(|error| BackupFailure::Failed {
        code: "restore.copy",
        message: format!("{} 恢复拷贝失败：{error}", entry.name),
    })?;
    set_owner_only_permissions(&restore_tmp)?;
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

fn set_directory_permissions(path: &Path) -> Result<(), BackupFailure> {
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(|error| {
        BackupFailure::Failed {
            code: "backup.permissions",
            message: format!("{} 权限设置失败：{error}", path.display()),
        }
    })
}

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
                    "- {}：schema 版本 {}，{} 字节，sha256 {}",
                    entry.name, entry.schema_version, entry.byte_count, entry.sha256,
                );
            }
            output.push_str(
                "说明：快照经 VACUUM INTO 一致性 cut，可随时用 wwc backup verify 校验。\n",
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
    use winwincode_server::UserAccountService;

    use crate::cli::WwcCliExit;

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

    fn chrono_now() -> winwincode_domain::Instant {
        winwincode_domain::Instant("2026-09-13T00:00:00.000Z".to_owned())
    }

    fn seed_server_store(data_directory: &Path) -> (String, String, String) {
        let admin = UserAccountService::open(data_directory).expect("account service");
        let owner = admin
            .initialize_owner("ops-owner", "ops-owner-password", &chrono_now())
            .expect("owner account");
        let user_id = owner.user_id.0.clone();
        let username = owner.username.clone();
        let temporary_password = "ops-owner-password".to_string();
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

    fn seed_device_store(data_directory: &Path) -> String {
        let mut store = DeviceStore::open(data_directory).expect("device store");
        let record = ensure_device_identity(&mut store, &device_seed(), "2026-09-05T00:00:00Z")
            .expect("device identity");
        let device_id = record.identity().device_id().to_owned();
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
        device_id
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

        {
            let database = connection(&data_directory.join("control-plane.sqlite3"));
            database
                .execute_batch(
                    "DELETE FROM projection_event_stream_heads; DELETE FROM outbox; \
                     DELETE FROM command_receipts; DELETE FROM owner_account;",
                )
                .expect("wipe live state");
        }
        assert_eq!(
            count(
                &data_directory,
                "control-plane.sqlite3",
                "SELECT count(*) FROM owner_account"
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

    fn assert_server_restored(
        data_directory: &Path,
        user_id: &str,
        username: &str,
        temporary_password: &str,
    ) {
        let service = UserAccountService::open(data_directory).expect("reopen storage");
        let verified = service
            .verify_credentials(username, temporary_password)
            .expect("verification runs")
            .expect("credential survives restore");
        assert_eq!(verified.user_id.0, user_id);
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

        let service = UserAccountService::open(&data_directory).expect("reopen storage");
        let verified = service
            .verify_credentials(&username, &temporary_password)
            .expect("verification runs")
            .expect("account still live");
        assert_eq!(verified.user_id.0, user_id);
    }

    #[test]
    fn restore_fails_closed_on_previous_control_plane_schema_version() {
        for previous_version in 0..SERVER_CONTROL_PLANE_SCHEMA_VERSION {
            let fixture = Fixture::new("server-previous-schema");
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
            patch_snapshot_version(&fixture.backup_dir(), "control-plane", previous_version);

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

            let service = UserAccountService::open(&data_directory).expect("reopen storage");
            let verified = service
                .verify_credentials(&username, &temporary_password)
                .expect("verification runs")
                .expect("account still live");
            assert_eq!(verified.user_id.0, user_id);
        }
    }

    #[test]
    fn restore_fails_closed_on_unsupported_device_schema_version() {
        let fixture = Fixture::new("device-newer-schema");
        let data = fixture.data_arg();
        let backup = fixture.backup_arg();
        let device_id = seed_device_store(&fixture.data_dir());
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
        patch_snapshot_version(
            &fixture.backup_dir(),
            "device-client",
            winwincode_device_client::CLIENT_STORE_SCHEMA_VERSION + 1,
        );

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
    }

    #[test]
    fn device_snapshot_and_restore_keep_identity_state() {
        let fixture = Fixture::new("device-round-trip");
        let data = fixture.data_arg();
        let backup = fixture.backup_arg();
        let device_id = seed_device_store(&fixture.data_dir());
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
    fn device_backup_restores_into_an_empty_directory() {
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
        assert_success(&restore);
        let store = DeviceStore::open(&other).expect("restored store");
        assert!(load_device_identity(&store).unwrap().is_some());
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
        let device_id = seed_device_store(&fixture.data_dir());
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

        let after = fs::read(data_directory.join("device-client.sqlite3")).expect("after bytes");
        assert_eq!(before, after);
        let store = DeviceStore::open(&data_directory).expect("store still opens");
        let record = load_device_identity(&store)
            .expect("identity load")
            .expect("identity present");
        assert_eq!(record.identity().device_id(), device_id);
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
                "SELECT count(*) FROM device_identity"
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
    fn supported_restore_versions_use_current_managed_schemas() {
        let server = supported_restore_versions(CONTROL_PLANE_DATABASE).expect("gated");
        assert_eq!(server, vec![SERVER_CONTROL_PLANE_SCHEMA_VERSION]);
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
