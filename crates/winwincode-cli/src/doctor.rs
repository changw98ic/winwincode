use std::fs;
use std::path::Path;

use winwincode_repository_context::{
    IndexCapability, LocalCodeIndexMode, LocalCodeIndexSnapshot, RepositoryContextPort,
    RepositoryContextQuery, RepositoryContextScanner,
};

use crate::git::{
    command_version, container_runtime, disk_available_kib, inspect_repository, total_memory_bytes,
};
use crate::{
    Attachment, DiagnosticCategory, DiagnosticCheck, DiagnosticReport, DiagnosticStatus,
    DoctorRequest, RepositoryInspection, SystemLocalLauncher,
};

pub(crate) fn build_diagnostic_report(
    launcher: &SystemLocalLauncher,
    request: &DoctorRequest,
) -> DiagnosticReport {
    let mut checks = vec![check(
        DiagnosticCategory::Product,
        DiagnosticStatus::Pass,
        "product.local-launcher",
        "本地启动器可以运行。",
        "wwc 通过 LocalLauncherPort 执行仓库操作。",
    )];
    checks.push(check_state_storage(launcher.state_root()));
    let (repository_root, repository_checks, local_code_index) =
        repository_diagnostics(launcher, &request.repository_path);
    checks.extend(repository_checks);
    checks.extend(environment_checks(
        repository_root
            .as_deref()
            .unwrap_or(&request.repository_path),
        launcher,
    ));
    checks.sort_by(|left, right| {
        left.category
            .cmp(&right.category)
            .then_with(|| left.code.cmp(&right.code))
    });
    let ok = !checks
        .iter()
        .any(|item| item.status == DiagnosticStatus::Error);
    DiagnosticReport {
        ok,
        checks,
        local_code_index,
    }
}

fn repository_diagnostics(
    launcher: &SystemLocalLauncher,
    requested_path: &Path,
) -> (
    Option<std::path::PathBuf>,
    Vec<DiagnosticCheck>,
    Option<LocalCodeIndexSnapshot>,
) {
    match inspect_repository(requested_path) {
        Ok(inspection) if inspection.git_initialized => {
            initialized_repository(launcher, &inspection)
        }
        Ok(_inspection) => (
            None,
            vec![check(
                DiagnosticCategory::Repository,
                DiagnosticStatus::Error,
                "repository.git-not-initialized",
                "该目录还不是 Git 仓库。",
                "运行 wwc init --confirm-git-init 后再选择 baseline。",
            )],
            None,
        ),
        Err(error) => (
            None,
            vec![check(
                error.category,
                DiagnosticStatus::Error,
                error.code,
                "仓库检查没有完成。",
                &error.message,
            )],
            None,
        ),
    }
}

fn initialized_repository(
    launcher: &SystemLocalLauncher,
    inspection: &RepositoryInspection,
) -> (
    Option<std::path::PathBuf>,
    Vec<DiagnosticCheck>,
    Option<LocalCodeIndexSnapshot>,
) {
    let Some(root) = inspection.repository_root.clone() else {
        return (
            None,
            vec![check(
                DiagnosticCategory::Repository,
                DiagnosticStatus::Error,
                "repository.root-missing",
                "Git 仓库根目录不可用。",
                "重新检查本地 Git 仓库。",
            )],
            None,
        );
    };
    let mut checks = vec![check(
        DiagnosticCategory::Repository,
        DiagnosticStatus::Pass,
        "repository.git-initialized",
        "Git 仓库已初始化。",
        &format!("仓库根目录：{}", root.display()),
    )];
    let (attachment_check, attachment) = attachment_diagnostic(launcher, &root);
    checks.push(attachment_check);
    let (baseline_checks, index) = baseline_diagnostics(&root, inspection, attachment.as_ref());
    checks.extend(baseline_checks);
    checks.extend(worktree_diagnostics(inspection));
    checks.extend(repository_hygiene_diagnostics(&root, inspection));
    checks.push(remote_diagnostic(inspection.remote_configured));
    (Some(root), checks, index)
}

fn attachment_diagnostic(
    launcher: &SystemLocalLauncher,
    root: &Path,
) -> (DiagnosticCheck, Option<Attachment>) {
    match launcher.read_attachment(root) {
        Ok(Some(attachment)) => (
            check(
                DiagnosticCategory::Product,
                DiagnosticStatus::Pass,
                "product.repository-attached",
                "仓库已经接入 WinWinCode。",
                &format!("已绑定 baseline {}。", attachment.baseline_sha),
            ),
            Some(attachment),
        ),
        Ok(None) => (
            check(
                DiagnosticCategory::Product,
                DiagnosticStatus::Warning,
                "product.repository-not-attached",
                "仓库尚未接入 WinWinCode。",
                "运行 wwc repo attach 并确认 baseline。",
            ),
            None,
        ),
        Err(error) => (
            check(
                DiagnosticCategory::Product,
                DiagnosticStatus::Error,
                error.code,
                "仓库绑定记录不可用。",
                &error.message,
            ),
            None,
        ),
    }
}

fn baseline_diagnostics(
    root: &Path,
    inspection: &RepositoryInspection,
    attachment: Option<&Attachment>,
) -> (Vec<DiagnosticCheck>, Option<LocalCodeIndexSnapshot>) {
    let mut checks = Vec::new();
    match (&inspection.head_sha, attachment) {
        (Some(head), _) => checks.push(check(
            DiagnosticCategory::Repository,
            DiagnosticStatus::Pass,
            "repository.head-valid",
            "仓库有有效 HEAD。",
            &format!("当前 HEAD：{head}"),
        )),
        (None, Some(binding)) => checks.push(check(
            DiagnosticCategory::Repository,
            DiagnosticStatus::Pass,
            "repository.snapshot-baseline-valid",
            "仓库使用已确认的 Snapshot baseline。",
            &format!("baseline：{}", binding.baseline_sha),
        )),
        (None, None) => checks.push(check(
            DiagnosticCategory::Repository,
            DiagnosticStatus::Error,
            "repository.head-missing",
            "仓库还没有有效 HEAD 或已绑定 Snapshot。",
            "运行 wwc repo attach --baseline snapshot --confirm-snapshot；不会修改当前分支。",
        )),
    }
    let baseline = attachment
        .map(|binding| binding.baseline_sha.as_str())
        .or(inspection.head_sha.as_deref());
    let Some(baseline) = baseline else {
        return (checks, None);
    };
    match RepositoryContextScanner::default().inspect(&RepositoryContextQuery::new(root, baseline))
    {
        Ok(context) => {
            checks.push(index_check(&context.local_code_index));
            (checks, Some(context.local_code_index))
        }
        Err(error) => {
            checks.push(check(
                DiagnosticCategory::Repository,
                DiagnosticStatus::Error,
                "repository.context-failed",
                "所选 baseline 的仓库信息读取失败。",
                &error.to_string(),
            ));
            (checks, None)
        }
    }
}

fn worktree_diagnostics(inspection: &RepositoryInspection) -> Vec<DiagnosticCheck> {
    let worktree = if inspection.dirty_paths.is_empty() {
        check(
            DiagnosticCategory::Repository,
            DiagnosticStatus::Pass,
            "repository.worktree-clean",
            "工作区没有未提交变化。",
            "可以直接使用 HEAD 作为 baseline。",
        )
    } else {
        check(
            DiagnosticCategory::Repository,
            DiagnosticStatus::Warning,
            "repository.worktree-dirty",
            "工作区有未提交变化。",
            &format!(
                "共 {} 个路径；接入时必须明确选择 head、snapshot 或 cancel。",
                inspection.dirty_paths.len()
            ),
        )
    };
    let secrets = if inspection.blocking_secret_paths.is_empty() {
        check(
            DiagnosticCategory::Repository,
            DiagnosticStatus::Pass,
            "repository.secret-paths-clear",
            "待处理变化中没有发现常见秘密文件名。",
            "该检查只按文件名识别；不会读取或输出秘密值。",
        )
    } else {
        check(
            DiagnosticCategory::Repository,
            DiagnosticStatus::Error,
            "repository.secret-paths-found",
            "发现疑似秘密文件，Snapshot 已被禁止。",
            &inspection.blocking_secret_paths.join(", "),
        )
    };
    vec![worktree, secrets]
}

fn repository_hygiene_diagnostics(
    root: &Path,
    inspection: &RepositoryInspection,
) -> Vec<DiagnosticCheck> {
    let gitignore = if root.join(".gitignore").is_file() {
        check(
            DiagnosticCategory::Repository,
            DiagnosticStatus::Pass,
            "repository.gitignore-present",
            "仓库包含 .gitignore。",
            "Snapshot 仍会单独阻止常见秘密文件名。",
        )
    } else {
        check(
            DiagnosticCategory::Repository,
            DiagnosticStatus::Warning,
            "repository.gitignore-missing",
            "仓库根目录没有 .gitignore。",
            "创建 Snapshot 前请先排除秘密、依赖目录和构建产物。",
        )
    };
    let risks = if inspection.risk_warnings.is_empty() {
        check(
            DiagnosticCategory::Repository,
            DiagnosticStatus::Pass,
            "repository.path-risks-clear",
            "没有发现常见秘密文件名、构建产物或大文件风险。",
            "检查覆盖 Git 跟踪文件和当前未提交路径。",
        )
    } else {
        check(
            DiagnosticCategory::Repository,
            DiagnosticStatus::Warning,
            "repository.path-risks-found",
            "发现需要查看的仓库路径风险。",
            &inspection.risk_warnings.join("；"),
        )
    };
    vec![gitignore, risks]
}

fn remote_diagnostic(configured: bool) -> DiagnosticCheck {
    check(
        DiagnosticCategory::Repository,
        DiagnosticStatus::Pass,
        if configured {
            "repository.remote-configured"
        } else {
            "repository.remote-optional"
        },
        if configured {
            "仓库配置了 Remote。"
        } else {
            "仓库没有 Remote，也可以继续本地交付。"
        },
        "诊断只记录是否存在 Remote，不读取或保存 Remote URL。",
    )
}

fn environment_checks(path: &Path, launcher: &SystemLocalLauncher) -> Vec<DiagnosticCheck> {
    let mut checks = Vec::new();
    checks.push(tool_check(
        "git",
        "environment.git",
        |_| true,
        "Git 可用。",
        "Git 未安装或不可执行。",
    ));
    checks.push(tool_check(
        "node",
        "environment.node",
        |version| version.starts_with("v24."),
        "Node.js 24 可用。",
        "需要 Node.js 24。",
    ));
    checks.push(tool_check(
        "rustc",
        "environment.rust",
        |version| version.starts_with("rustc 1.95."),
        "Rust 1.95 可用。",
        "需要 Rust 1.95。",
    ));

    let cpu_count = std::thread::available_parallelism().map_or(0, std::num::NonZero::get);
    checks.push(check(
        DiagnosticCategory::Environment,
        if cpu_count >= 2 {
            DiagnosticStatus::Pass
        } else {
            DiagnosticStatus::Warning
        },
        "environment.cpu",
        if cpu_count >= 2 {
            "CPU 资源可用于本地执行。"
        } else {
            "可用 CPU 较少，本地并行执行会受限。"
        },
        &format!("可用逻辑 CPU：{cpu_count}"),
    ));
    checks.push(memory_check());
    checks.push(disk_check(path));

    let container = container_runtime();
    checks.push(check(
        DiagnosticCategory::Environment,
        DiagnosticStatus::Pass,
        "environment.container",
        container.as_ref().map_or(
            "未检测到容器运行时；Community 本地流程不依赖容器。",
            |_| "检测到容器运行时。",
        ),
        container.as_deref().unwrap_or("Docker/Podman 为可选能力。"),
    ));
    if launcher.provider_variables().is_empty() {
        checks.push(check(
            DiagnosticCategory::Environment,
            DiagnosticStatus::Warning,
            "environment.provider-not-configured",
            "尚未检测到模型 Provider 配置。",
            "仓库初始化和本地诊断仍可使用；开始模型执行前再配置 Provider。",
        ));
    } else {
        checks.push(check(
            DiagnosticCategory::Environment,
            DiagnosticStatus::Pass,
            "environment.provider-configured",
            "检测到模型 Provider 配置引用。",
            &format!(
                "只检查变量名，不读取值：{}",
                launcher
                    .provider_variables()
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        ));
    }
    checks
}

fn check_state_storage(root: &Path) -> DiagnosticCheck {
    let probe = root.join(format!(".doctor-write-probe-{}", std::process::id()));
    let result = fs::create_dir_all(root)
        .and_then(|()| fs::write(&probe, b""))
        .and_then(|()| fs::remove_file(&probe));
    match result {
        Ok(()) => check(
            DiagnosticCategory::Product,
            DiagnosticStatus::Pass,
            "product.state-storage",
            "本地状态目录可写。",
            &format!("目录：{}", root.display()),
        ),
        Err(error) => check(
            DiagnosticCategory::Product,
            DiagnosticStatus::Error,
            "product.state-storage",
            "本地状态目录不可写。",
            &format!("目录：{}；{error}", root.display()),
        ),
    }
}

fn tool_check(
    program: &str,
    code: &str,
    accepted: impl FnOnce(&str) -> bool,
    pass_message: &str,
    fail_message: &str,
) -> DiagnosticCheck {
    match command_version(program) {
        Ok(version) if accepted(&version) => check(
            DiagnosticCategory::Environment,
            DiagnosticStatus::Pass,
            code,
            pass_message,
            &version,
        ),
        Ok(version) => check(
            DiagnosticCategory::Environment,
            DiagnosticStatus::Error,
            code,
            fail_message,
            &format!("当前版本：{version}"),
        ),
        Err(error) => check(
            DiagnosticCategory::Environment,
            DiagnosticStatus::Error,
            code,
            fail_message,
            &error.message,
        ),
    }
}

fn memory_check() -> DiagnosticCheck {
    let Some(bytes) = total_memory_bytes() else {
        return check(
            DiagnosticCategory::Environment,
            DiagnosticStatus::Warning,
            "environment.memory",
            "没有读到系统内存总量。",
            "这不会阻止仓库接入，但执行前需要关注本机资源。",
        );
    };
    let gib = gibibytes_tenths_from_bytes(bytes);
    check(
        DiagnosticCategory::Environment,
        if bytes >= 2 * 1024 * 1024 * 1024 {
            DiagnosticStatus::Pass
        } else {
            DiagnosticStatus::Warning
        },
        "environment.memory",
        if bytes >= 2 * 1024 * 1024 * 1024 {
            "系统内存满足基础本地运行检查。"
        } else {
            "系统内存较少，本地执行可能受限。"
        },
        &format!("总内存约 {}.{} GiB。", gib.0, gib.1),
    )
}

fn disk_check(path: &Path) -> DiagnosticCheck {
    let Some(kib) = disk_available_kib(path) else {
        return check(
            DiagnosticCategory::Environment,
            DiagnosticStatus::Warning,
            "environment.disk",
            "没有读到可用磁盘空间。",
            "这不会阻止仓库接入，但执行和 Evidence 导出前需要确认空间。",
        );
    };
    let gib = gibibytes_tenths_from_kib(kib);
    check(
        DiagnosticCategory::Environment,
        if kib >= 1024 * 1024 {
            DiagnosticStatus::Pass
        } else {
            DiagnosticStatus::Warning
        },
        "environment.disk",
        if kib >= 1024 * 1024 {
            "磁盘空间满足基础本地运行检查。"
        } else {
            "可用磁盘空间低于 1 GiB。"
        },
        &format!("可用空间约 {}.{} GiB。", gib.0, gib.1),
    )
}

fn gibibytes_tenths_from_bytes(bytes: u64) -> (u64, u64) {
    let mebibytes = bytes / (1024 * 1024);
    (mebibytes / 1024, (mebibytes % 1024) * 10 / 1024)
}

fn gibibytes_tenths_from_kib(kib: u64) -> (u64, u64) {
    let mebibytes = kib / 1024;
    (mebibytes / 1024, (mebibytes % 1024) * 10 / 1024)
}

fn index_check(index: &LocalCodeIndexSnapshot) -> DiagnosticCheck {
    let mode = match index.mode {
        LocalCodeIndexMode::AstGrepOutline => "ast-grep-outline",
        LocalCodeIndexMode::GitFileInventory => "git-file-inventory",
    };
    let capabilities = index
        .capabilities
        .supported
        .iter()
        .map(|capability| capability_name(*capability))
        .collect::<Vec<_>>()
        .join(", ");
    check(
        DiagnosticCategory::Repository,
        if index.available && index.fresh {
            DiagnosticStatus::Pass
        } else {
            DiagnosticStatus::Warning
        },
        "repository.local-code-index",
        if index.fresh {
            "本地代码索引与 baseline 一致。"
        } else {
            "本地代码索引不是最新状态。"
        },
        &format!(
            "mode={mode}；fresh={}；baseline={}；capability={}；{}",
            index.fresh, index.baseline_sha, capabilities, index.detail
        ),
    )
}

fn capability_name(capability: IndexCapability) -> &'static str {
    match capability {
        IndexCapability::FilePaths => "file-paths",
        IndexCapability::Languages => "languages",
        IndexCapability::Sizes => "sizes",
        IndexCapability::ContentFingerprints => "content-fingerprints",
        IndexCapability::SymbolOutlines => "symbol-outlines",
        IndexCapability::Callers => "callers",
        IndexCapability::Callees => "callees",
        IndexCapability::DependencyGraph => "dependency-graph",
        IndexCapability::TestRelations => "test-relations",
    }
}

fn check(
    category: DiagnosticCategory,
    status: DiagnosticStatus,
    code: &str,
    message: &str,
    detail: &str,
) -> DiagnosticCheck {
    DiagnosticCheck {
        category,
        status,
        code: code.to_owned(),
        message: message.to_owned(),
        detail: detail.to_owned(),
    }
}
