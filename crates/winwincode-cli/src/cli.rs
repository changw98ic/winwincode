use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::PathBuf;

use serde::Serialize;
use winwincode_domain::{UserAccountRole, UserAccountState};

use crate::device_admin::{
    DeviceAdminError, DeviceAdminOutcome, device_status, refresh_device_connect_code,
    set_device_lock,
};
use crate::user_admin::{UserAccountAdmin, UserAdminError, UserAdminOutcome};
use crate::{
    AttachRequest, BaselineChoice, DiagnosticCategory, DiagnosticReport, DiagnosticStatus,
    DoctorRequest, InitRequest, LauncherError, LocalLauncherPort, SetupOutcome,
};

const EXIT_SUCCESS: i32 = 0;
const EXIT_DIAGNOSTIC_FAILED: i32 = 1;
const EXIT_USAGE: i32 = 2;
const EXIT_ACTION_REQUIRED: i32 = 3;
const EXIT_SERVICE: i32 = 5;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WwcCliExit {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Debug)]
struct ParsedArguments {
    positionals: Vec<String>,
    flags: BTreeMap<String, Vec<String>>,
    switches: BTreeSet<String>,
}

#[derive(Debug)]
struct UsageError(String);

pub fn run_cli(arguments: &[String], launcher: &dyn LocalLauncherPort) -> WwcCliExit {
    match run(arguments, launcher) {
        Ok(outcome) => outcome,
        Err(error) => WwcCliExit {
            code: EXIT_USAGE,
            stdout: String::new(),
            stderr: format!("参数错误：{}\n\n{}", error.0, render_help()),
        },
    }
}

pub fn render_help() -> String {
    [
        "WinWinCode 本地命令：",
        "  wwc init [PATH] [--confirm-git-init] [--baseline head|snapshot|cancel] [--confirm-snapshot] [--json]",
        "  wwc repo attach [PATH] [--baseline head|snapshot|cancel] [--confirm-snapshot] [--json]",
        "  wwc doctor [PATH] [--json]",
        "  wwc user create <USERNAME> [--role owner|member] [--data-dir PATH] [--json]",
        "  wwc user disable <USERNAME> [--data-dir PATH] [--json]",
        "  wwc user enable <USERNAME> [--data-dir PATH] [--json]",
        "  wwc user reset-password <USERNAME> [--data-dir PATH] [--json]",
        "  wwc device status|refresh-code|lock|unlock [--data-dir PATH] [--json]",
        "  wwc help",
        "",
        "Git 初始化和 Snapshot 都需要显式确认。Snapshot 使用专用 ref，不会修改当前分支、索引或 stash。",
        "没有 Remote 也可以接入和完成本地交付。",
        "用户管理直接操作 Server 产品状态数据库：--data-dir 与 Server 的 WWC_SERVER_DATA_DIRECTORY 一致。",
        "临时密码只显示一次，绝不再次展示。禁用用户不触达浏览器会话：会话撤销由 Server 负责，",
        "与正在运行的 Server 共库时需重启 Server 或经 HTTP 端点操作才即时生效。",
        "device 命令操作 Device Client 本地数据目录：--data-dir 为 Device Client 的数据目录。",
        "动态连接码明文只在 refresh-code 时显示一次，绝不写入日志或数据库；锁定期间新的连接验证一律拒绝。",
        "",
    ]
    .join("\n")
}

fn run(arguments: &[String], launcher: &dyn LocalLauncherPort) -> Result<WwcCliExit, UsageError> {
    let Some(command) = arguments.first().map(String::as_str) else {
        return Ok(WwcCliExit {
            code: EXIT_SUCCESS,
            stdout: render_help(),
            stderr: String::new(),
        });
    };
    if matches!(command, "help" | "--help" | "-h") {
        if arguments.len() != 1 {
            return Err(UsageError("help 不接受其他参数。".into()));
        }
        return Ok(WwcCliExit {
            code: EXIT_SUCCESS,
            stdout: render_help(),
            stderr: String::new(),
        });
    }
    match command {
        "init" => run_init(&arguments[1..], launcher),
        "repo" => run_repo(&arguments[1..], launcher),
        "doctor" => run_doctor(&arguments[1..], launcher),
        "user" => run_user(&arguments[1..]),
        "device" => run_device(&arguments[1..]),
        other => Err(UsageError(format!("未知命令 {other}。"))),
    }
}

fn run_init(
    arguments: &[String],
    launcher: &dyn LocalLauncherPort,
) -> Result<WwcCliExit, UsageError> {
    let parsed = parse(arguments, &["confirm-git-init", "confirm-snapshot", "json"])?;
    reject_unknown(
        &parsed,
        &["baseline"],
        &["confirm-git-init", "confirm-snapshot", "json"],
    )?;
    let request = InitRequest {
        repository_path: repository_path(&parsed)?,
        confirm_git_init: parsed.switches.contains("confirm-git-init"),
        baseline: baseline_choice(&parsed)?,
        confirm_snapshot: parsed.switches.contains("confirm-snapshot"),
    };
    Ok(setup_exit(
        launcher.initialize_repository(&request),
        parsed.switches.contains("json"),
    ))
}

fn run_repo(
    arguments: &[String],
    launcher: &dyn LocalLauncherPort,
) -> Result<WwcCliExit, UsageError> {
    let Some(action) = arguments.first() else {
        return Err(UsageError("repo 后需要 attach。".into()));
    };
    if action != "attach" {
        return Err(UsageError(format!("未知 repo 命令 {action}。")));
    }
    let parsed = parse(&arguments[1..], &["confirm-snapshot", "json"])?;
    reject_unknown(&parsed, &["baseline"], &["confirm-snapshot", "json"])?;
    let request = AttachRequest {
        repository_path: repository_path(&parsed)?,
        baseline: baseline_choice(&parsed)?,
        confirm_snapshot: parsed.switches.contains("confirm-snapshot"),
    };
    Ok(setup_exit(
        launcher.attach_repository(&request),
        parsed.switches.contains("json"),
    ))
}

fn run_doctor(
    arguments: &[String],
    launcher: &dyn LocalLauncherPort,
) -> Result<WwcCliExit, UsageError> {
    let parsed = parse(arguments, &["json"])?;
    reject_unknown(&parsed, &[], &["json"])?;
    let json = parsed.switches.contains("json");
    match launcher.doctor(&DoctorRequest {
        repository_path: repository_path(&parsed)?,
    }) {
        Ok(report) => Ok(WwcCliExit {
            code: if report.ok {
                EXIT_SUCCESS
            } else {
                EXIT_DIAGNOSTIC_FAILED
            },
            stdout: if json {
                render_json(&report)
            } else {
                render_doctor(&report)
            },
            stderr: String::new(),
        }),
        Err(error) => Ok(error_exit(&error)),
    }
}

/// Parses and runs one `wwc user ...` administration command against the
/// Server product-state directory.
fn run_user(arguments: &[String]) -> Result<WwcCliExit, UsageError> {
    let Some(action) = arguments.first().map(String::as_str) else {
        return Err(UsageError(
            "user 后需要 create、disable、enable 或 reset-password。".into(),
        ));
    };
    if !matches!(action, "create" | "disable" | "enable" | "reset-password") {
        return Err(UsageError(format!("未知 user 命令 {action}。")));
    }
    let parsed = parse(&arguments[1..], &["json"])?;
    reject_unknown(&parsed, &["data-dir", "role"], &["json"])?;
    let data_directory = data_directory(&parsed)?;
    let json = parsed.switches.contains("json");
    let admin = UserAccountAdmin::open(data_directory);
    let result = match action {
        "create" => {
            let username = single_username(&parsed)?;
            admin.create(username, user_role(&parsed)?)
        }
        "disable" => admin.set_state(single_username(&parsed)?, UserAccountState::Disabled),
        "enable" => admin.set_state(single_username(&parsed)?, UserAccountState::Active),
        _ => admin.reset_password(single_username(&parsed)?),
    };
    Ok(user_exit(result, json))
}

fn user_exit(result: Result<UserAdminOutcome, UserAdminError>, json: bool) -> WwcCliExit {
    match result {
        Ok(outcome) => WwcCliExit {
            code: EXIT_SUCCESS,
            stdout: if json {
                render_json(&outcome)
            } else {
                render_user(&outcome)
            },
            stderr: String::new(),
        },
        Err(UserAdminError::InitializationRequired) => WwcCliExit {
            code: EXIT_ACTION_REQUIRED,
            stdout: if json {
                render_json(&UserAdminOutcome::InitializationRequired)
            } else {
                render_initialization_guidance()
            },
            stderr: String::new(),
        },
        Err(UserAdminError::Failed { code, message }) => WwcCliExit {
            code: EXIT_SERVICE,
            stdout: String::new(),
            stderr: format!("用户管理问题 [{code}]：{message}\n"),
        },
    }
}

fn render_user(outcome: &UserAdminOutcome) -> String {
    match outcome {
        UserAdminOutcome::UserCreated {
            user,
            temporary_password,
        } => format!(
            "用户已创建。\n用户：{}\nID：{}\n角色：{}\n临时密码：{temporary_password}\n说明：临时密码只显示这一次，请立即通过安全渠道转交该用户；关闭本输出后无法找回。\n",
            user.username,
            user.user_id,
            user.role.as_str()
        ),
        UserAdminOutcome::PasswordReset {
            user,
            temporary_password,
        } => format!(
            "密码已重置。\n用户：{}\nID：{}\n新临时密码：{temporary_password}\n说明：新临时密码只显示这一次，请立即转交该用户；原密码立即失效。\n",
            user.username, user.user_id
        ),
        UserAdminOutcome::UserUpdated { user, changed } => render_user_update(user, *changed),
        UserAdminOutcome::InitializationRequired => render_initialization_guidance(),
    }
}

fn render_user_update(user: &crate::user_admin::UserAccountView, changed: bool) -> String {
    let (headline, state) = match user.state {
        UserAccountState::Disabled => ("用户已禁用。", "禁用"),
        UserAccountState::Active => ("用户已启用。", "启用"),
    };
    let headline = if changed {
        headline.to_owned()
    } else {
        format!("用户已处于{state}状态，未做修改。")
    };
    let mut output = format!(
        "{headline}\n用户：{}\nID：{}\n",
        user.username, user.user_id
    );
    if user.state == UserAccountState::Disabled {
        output
            .push_str("说明：浏览器会话撤销由 Server 负责，CLI 直连数据库路径，不触达在线会话。\n");
        output.push_str("注意：如 Server 正在运行并与本目录共用数据库，已登录会话不会立即失效；需重启 Server 或改经 HTTP 端点操作才即时生效。\n");
    }
    output
}

fn render_initialization_guidance() -> String {
    [
        "Server 尚未初始化：该数据目录还没有 Owner。",
        "请先通过浏览器完成一次性初始化（在 Server 登录页输入 bootstrap proof），或运行：",
        "  wwc user create <USERNAME> --role owner --data-dir <数据目录>",
        "",
    ]
    .join("\n")
}

/// Parses and runs one `wwc device ...` command against the Device Client
/// local data directory (plan 16.8: the CLI is the no-desktop fallback).
fn run_device(arguments: &[String]) -> Result<WwcCliExit, UsageError> {
    let Some(action) = arguments.first().map(String::as_str) else {
        return Err(UsageError(
            "device 后需要 status、refresh-code、lock 或 unlock。".into(),
        ));
    };
    if !matches!(action, "status" | "refresh-code" | "lock" | "unlock") {
        return Err(UsageError(format!("未知 device 命令 {action}。")));
    }
    let parsed = parse(&arguments[1..], &["json"])?;
    reject_unknown(&parsed, &["data-dir"], &["json"])?;
    let data_directory = device_data_directory(&parsed)?;
    let json = parsed.switches.contains("json");
    let result = match action {
        "status" => device_status(&data_directory),
        "refresh-code" => refresh_device_connect_code(&data_directory),
        "lock" => set_device_lock(&data_directory, true),
        "unlock" => set_device_lock(&data_directory, false),
        _ => unreachable!("action vocabulary is checked above"),
    };
    Ok(device_exit(result, json))
}

fn device_data_directory(parsed: &ParsedArguments) -> Result<PathBuf, UsageError> {
    let values = parsed.flags.get("data-dir").map_or(&[][..], Vec::as_slice);
    if values.len() > 1 {
        return Err(UsageError("--data-dir 不能重复。".into()));
    }
    values.first().map(PathBuf::from).ok_or_else(|| {
        UsageError("缺少 --data-dir：device 命令需要 Device Client 的本地数据目录。".into())
    })
}

fn device_exit(result: Result<DeviceAdminOutcome, DeviceAdminError>, json: bool) -> WwcCliExit {
    match result {
        Ok(outcome) => WwcCliExit {
            code: EXIT_SUCCESS,
            stdout: if json {
                render_json(&outcome)
            } else {
                render_device(&outcome)
            },
            stderr: String::new(),
        },
        Err(DeviceAdminError::NotInitialized) => WwcCliExit {
            code: EXIT_ACTION_REQUIRED,
            stdout: if json {
                "{\"status\":\"not-initialized\"}\n".to_owned()
            } else {
                render_device_initialization_guidance()
            },
            stderr: String::new(),
        },
        Err(DeviceAdminError::NotEnrolled) => WwcCliExit {
            code: EXIT_ACTION_REQUIRED,
            stdout: if json {
                "{\"status\":\"not-enrolled\"}\n".to_owned()
            } else {
                render_device_enrollment_guidance()
            },
            stderr: String::new(),
        },
        Err(DeviceAdminError::Failed { code, message }) => WwcCliExit {
            code: EXIT_SERVICE,
            stdout: String::new(),
            stderr: format!("device 命令失败 [{code}]：{message}\n"),
        },
    }
}

fn render_device(outcome: &DeviceAdminOutcome) -> String {
    match outcome {
        DeviceAdminOutcome::Status { device } => {
            let mut output = format!("WinWinCode Device\n设备 ID：{}\n", device.device_id);
            if device.enrolled {
                let _ = write!(
                    output,
                    "Client ID：{}\n节点 ID：{}\n",
                    device.public_client_id, device.client_node_id
                );
            } else {
                output.push_str("注册状态：未完成 enrollment（等待 Server 接受）\n");
            }
            output.push_str(if device.accepting_connections {
                "连接状态：接受新连接\n"
            } else {
                "连接状态：不接受新连接\n"
            });
            output.push_str(if device.lock_state == "locked" {
                "锁定状态：已锁定\n"
            } else {
                "锁定状态：未锁定\n"
            });
            match &device.connect_code {
                Some(code) => {
                    let _ = write!(
                        output,
                        "动态连接码：第 {} 代（{}）\n连接码编号：{}\n有效期至：{}",
                        code.generation, code.state, code.connect_code_id, code.expires_at
                    );
                    match code.remaining_seconds {
                        Some(seconds @ 0..) => {
                            let _ = writeln!(output, "（剩余 {seconds} 秒）");
                        }
                        _ => output.push_str("（已过期）\n"),
                    }
                    output.push_str(
                        "说明：状态页不显示明文连接码；明文只在 refresh-code 时显示一次。\n",
                    );
                }
                None => output.push_str("动态连接码：未发布\n"),
            }
            output
        }
        DeviceAdminOutcome::CodeRefreshed {
            code,
            connect_code,
            valid_seconds,
        } => format!(
            "动态连接码已生成。\n动态连接码：{} {}\n有效期至：{}（{valid_seconds} 秒）\n说明：明文连接码只显示这一次，请立即在 Web 端输入；关闭本输出后无法找回。\
             旧连接码已立即失效，发布帧将在 Device Client 下一次交换时送达 Server。\n",
            &connect_code[..connect_code.len() / 2],
            &connect_code[connect_code.len() / 2..],
            code.expires_at
        ),
        DeviceAdminOutcome::PolicyUpdated {
            accepting_connections,
            lock_state,
        } => {
            let headline = if lock_state == "locked" {
                "Client 已锁定。"
            } else {
                "Client 已解锁。"
            };
            let connections = if *accepting_connections {
                "接受新连接"
            } else {
                "不接受新连接"
            };
            format!(
                "{headline}\n连接状态：{connections}\n说明：锁定期间新的连接验证请求一律拒绝；策略已持久化，并随后续 hello/心跳上报 Server。\n"
            )
        }
    }
}

fn render_device_initialization_guidance() -> String {
    [
        "Device Client 尚未初始化：该数据目录还没有设备身份。",
        "请先启动 WinWinCode Device Client（它会生成本地身份并向 Server 注册），",
        "或用 --data-dir 指向 Device Client 的数据目录。",
        "",
    ]
    .join("\n")
}

fn render_device_enrollment_guidance() -> String {
    [
        "设备尚未完成 enrollment 注册：请先启动 Device Client 与 Server 完成一次交换。",
        "注册接受后才可发布动态连接码（否则发布帧无法挂到分配的节点流上）。",
        "",
    ]
    .join("\n")
}

fn data_directory(parsed: &ParsedArguments) -> Result<PathBuf, UsageError> {
    let values = parsed.flags.get("data-dir").map_or(&[][..], Vec::as_slice);
    if values.len() > 1 {
        return Err(UsageError("--data-dir 不能重复。".into()));
    }
    if let Some(value) = values.first() {
        return Ok(PathBuf::from(value));
    }
    if let Some(value) = std::env::var_os("WWC_SERVER_DATA_DIRECTORY") {
        return Ok(PathBuf::from(value));
    }
    Err(UsageError(
        "缺少 --data-dir：用户管理需要 Server 产品状态目录（与 Server 的 WWC_SERVER_DATA_DIRECTORY 相同）。".into(),
    ))
}

fn user_role(parsed: &ParsedArguments) -> Result<UserAccountRole, UsageError> {
    let values = parsed.flags.get("role").map_or(&[][..], Vec::as_slice);
    if values.len() > 1 {
        return Err(UsageError("--role 不能重复。".into()));
    }
    match values.first().map(String::as_str) {
        None | Some("member") => Ok(UserAccountRole::Member),
        Some("owner") => Ok(UserAccountRole::Owner),
        Some(other) => Err(UsageError(format!(
            "--role 只能是 owner 或 member，收到 {other}。"
        ))),
    }
}

fn single_username(parsed: &ParsedArguments) -> Result<&str, UsageError> {
    if parsed.positionals.len() != 1 {
        return Err(UsageError("需要且只需要一个用户名。".into()));
    }
    Ok(parsed
        .positionals
        .first()
        .map(String::as_str)
        .unwrap_or_default())
}

fn setup_exit(result: Result<SetupOutcome, LauncherError>, json: bool) -> WwcCliExit {
    match result {
        Ok(outcome) => {
            let action_required = matches!(
                outcome,
                SetupOutcome::GitInitializationConfirmationRequired { .. }
                    | SetupOutcome::BaselineChoiceRequired { .. }
                    | SetupOutcome::SnapshotConfirmationRequired { .. }
            );
            WwcCliExit {
                code: if action_required {
                    EXIT_ACTION_REQUIRED
                } else {
                    EXIT_SUCCESS
                },
                stdout: if json {
                    render_json(&outcome)
                } else {
                    render_setup(&outcome)
                },
                stderr: String::new(),
            }
        }
        Err(error) => error_exit(&error),
    }
}

fn parse(arguments: &[String], switches: &[&str]) -> Result<ParsedArguments, UsageError> {
    let switch_names = switches.iter().copied().collect::<BTreeSet<_>>();
    let mut parsed = ParsedArguments {
        positionals: Vec::new(),
        flags: BTreeMap::new(),
        switches: BTreeSet::new(),
    };
    let mut index = 0;
    while index < arguments.len() {
        let token = &arguments[index];
        if !token.starts_with("--") {
            parsed.positionals.push(token.clone());
            index += 1;
            continue;
        }
        let option = &token[2..];
        if option.is_empty() {
            return Err(UsageError("选项名称不能为空。".into()));
        }
        if let Some((name, value)) = option.split_once('=') {
            if switch_names.contains(name) {
                return Err(UsageError(format!("--{name} 不接受值。")));
            }
            parsed
                .flags
                .entry(name.to_owned())
                .or_default()
                .push(value.to_owned());
            index += 1;
            continue;
        }
        if switch_names.contains(option) {
            if !parsed.switches.insert(option.to_owned()) {
                return Err(UsageError(format!("--{option} 不能重复。")));
            }
            index += 1;
            continue;
        }
        let Some(value) = arguments.get(index + 1) else {
            return Err(UsageError(format!("--{option} 缺少值。")));
        };
        if value.starts_with("--") {
            return Err(UsageError(format!("--{option} 缺少值。")));
        }
        parsed
            .flags
            .entry(option.to_owned())
            .or_default()
            .push(value.clone());
        index += 2;
    }
    Ok(parsed)
}

fn reject_unknown(
    parsed: &ParsedArguments,
    flags: &[&str],
    switches: &[&str],
) -> Result<(), UsageError> {
    if let Some(name) = parsed
        .flags
        .keys()
        .find(|name| !flags.contains(&name.as_str()))
    {
        return Err(UsageError(format!("未知选项 --{name}。")));
    }
    if let Some(name) = parsed
        .switches
        .iter()
        .find(|name| !switches.contains(&name.as_str()))
    {
        return Err(UsageError(format!("未知选项 --{name}。")));
    }
    Ok(())
}

fn repository_path(parsed: &ParsedArguments) -> Result<PathBuf, UsageError> {
    if parsed.positionals.len() > 1 {
        return Err(UsageError("最多只能提供一个仓库路径。".into()));
    }
    Ok(parsed
        .positionals
        .first()
        .map_or_else(|| PathBuf::from("."), PathBuf::from))
}

fn baseline_choice(parsed: &ParsedArguments) -> Result<Option<BaselineChoice>, UsageError> {
    let values = parsed.flags.get("baseline").map_or(&[][..], Vec::as_slice);
    if values.len() > 1 {
        return Err(UsageError("--baseline 不能重复。".into()));
    }
    values
        .first()
        .map(|value| match value.as_str() {
            "head" => Ok(BaselineChoice::Head),
            "snapshot" => Ok(BaselineChoice::Snapshot),
            "cancel" => Ok(BaselineChoice::Cancel),
            _ => Err(UsageError(
                "--baseline 只能是 head、snapshot 或 cancel。".into(),
            )),
        })
        .transpose()
}

fn render_setup(outcome: &SetupOutcome) -> String {
    match outcome {
        SetupOutcome::Ready { attachment } => format!(
            "仓库已接入。\n仓库：{}\nbaseline：{}\n来源：{}\nRemote：{}\n本地绑定：{}\n",
            attachment.attachment.repository_root,
            attachment.attachment.baseline_sha,
            match attachment.attachment.baseline_source {
                crate::BaselineSource::Head => "HEAD",
                crate::BaselineSource::SnapshotRef => "专用 Snapshot ref",
            },
            if attachment.attachment.remote_configured {
                "已配置"
            } else {
                "未配置，不影响本地交付"
            },
            if attachment.state_changed {
                "已保存"
            } else {
                "内容未变化"
            }
        ),
        SetupOutcome::GitInitializationConfirmationRequired { repository_path } => format!(
            "目录还不是 Git 仓库：{repository_path}\n确认后运行：wwc init {repository_path} --confirm-git-init\n"
        ),
        SetupOutcome::BaselineChoiceRequired {
            repository_root,
            head_available,
            dirty_paths,
            risk_warnings,
            choices,
        } => format!(
            "需要选择 baseline。\n仓库：{repository_root}\nHEAD：{}\n未提交路径：{}\n可选：{}\n{}",
            if *head_available {
                "可用"
            } else {
                "不存在"
            },
            dirty_paths.len(),
            choices.join("、"),
            render_warnings(risk_warnings),
        ),
        SetupOutcome::SnapshotConfirmationRequired {
            repository_root,
            dirty_paths,
            risk_warnings,
        } => format!(
            "Snapshot 尚未创建。\n仓库：{repository_root}\n将检查并收录 {} 个变化路径。确认后加 --confirm-snapshot。\n{}",
            dirty_paths.len(),
            render_warnings(risk_warnings),
        ),
        SetupOutcome::Cancelled { repository_root } => {
            format!("已取消仓库接入：{repository_root}\n")
        }
    }
}

fn render_warnings(warnings: &[String]) -> String {
    if warnings.is_empty() {
        return "没有发现文件名、构建产物或大文件风险。\n".into();
    }
    format!("注意：\n- {}\n", warnings.join("\n- "))
}

fn render_doctor(report: &DiagnosticReport) -> String {
    let mut output = String::new();
    for category in [
        DiagnosticCategory::Product,
        DiagnosticCategory::Repository,
        DiagnosticCategory::Environment,
    ] {
        output.push_str(match category {
            DiagnosticCategory::Product => "产品检查\n",
            DiagnosticCategory::Repository => "仓库检查\n",
            DiagnosticCategory::Environment => "环境检查\n",
        });
        for check in report
            .checks
            .iter()
            .filter(|check| check.category == category)
        {
            let status = match check.status {
                DiagnosticStatus::Pass => "通过",
                DiagnosticStatus::Warning => "提醒",
                DiagnosticStatus::Error => "问题",
            };
            let _ = write!(
                output,
                "  [{status}] {}\n         {}\n",
                check.message, check.detail
            );
        }
    }
    output.push_str(if report.ok {
        "结果：没有发现阻止本地运行的问题。\n"
    } else {
        "结果：存在需要处理的问题，详情见上方“问题”项。\n"
    });
    output
}

fn render_json(value: &impl Serialize) -> String {
    serde_json::to_string_pretty(value).map_or_else(
        |error| {
            format!(
                "{{\"status\":\"serialization-error\",\"detail\":{}}}\n",
                serde_json::to_string(&error.to_string()).unwrap_or_else(|_| "null".into())
            )
        },
        |json| format!("{json}\n"),
    )
}

fn error_exit(error: &LauncherError) -> WwcCliExit {
    let category = match error.category {
        DiagnosticCategory::Product => "产品",
        DiagnosticCategory::Repository => "仓库",
        DiagnosticCategory::Environment => "环境",
    };
    WwcCliExit {
        code: EXIT_SERVICE,
        stdout: String::new(),
        stderr: format!("{category}问题 [{}]：{}\n", error.code, error.message),
    }
}
