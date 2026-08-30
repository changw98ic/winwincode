use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::PathBuf;

use serde::Serialize;

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
        "  wwc help",
        "",
        "Git 初始化和 Snapshot 都需要显式确认。Snapshot 使用专用 ref，不会修改当前分支、索引或 stash。",
        "没有 Remote 也可以接入和完成本地交付。",
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
