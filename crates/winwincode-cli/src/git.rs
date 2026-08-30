use std::collections::BTreeSet;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::{LauncherError, RepositoryInspection};

const LARGE_FILE_BYTES: u64 = 10 * 1024 * 1024;
const MAX_PRE_INIT_FILES: usize = 100_000;
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SnapshotBaseline {
    pub commit_sha: String,
    pub reference: String,
}

pub(crate) fn inspect_repository(path: &Path) -> Result<RepositoryInspection, LauncherError> {
    let requested_path = fs::canonicalize(path).map_err(|error| {
        LauncherError::repository(
            "repository.path-unavailable",
            format!("无法打开目录 {}：{error}", path.display()),
        )
    })?;
    if !requested_path.is_dir() {
        return Err(LauncherError::repository(
            "repository.path-not-directory",
            format!("{} 不是目录。", requested_path.display()),
        ));
    }

    let root_output = git(&requested_path, ["rev-parse", "--show-toplevel"])?;
    if !root_output.status.success() {
        let paths = worktree_paths_without_git(&requested_path)?;
        let risks = path_risks(&requested_path, &paths);
        return Ok(RepositoryInspection {
            requested_path,
            repository_root: None,
            git_initialized: false,
            head_sha: None,
            current_branch: None,
            dirty_paths: paths,
            risk_warnings: risks.warnings,
            blocking_secret_paths: risks.secrets,
            remote_configured: false,
        });
    }

    let root_text = String::from_utf8_lossy(&root_output.stdout)
        .trim()
        .to_owned();
    let repository_root = fs::canonicalize(&root_text).map_err(|error| {
        LauncherError::repository(
            "repository.root-unavailable",
            format!("Git 返回的仓库根目录 {root_text} 不可用：{error}"),
        )
    })?;
    let head_sha = successful_text(&git(
        &repository_root,
        ["rev-parse", "--verify", "HEAD^{commit}"],
    )?);
    let current_branch = successful_text(&git(
        &repository_root,
        ["symbolic-ref", "--quiet", "--short", "HEAD"],
    )?);
    let dirty_paths = dirty_paths(&repository_root)?;
    let mut candidate_paths = tracked_paths(&repository_root)?;
    candidate_paths.extend(dirty_paths.iter().cloned());
    candidate_paths.sort();
    candidate_paths.dedup();
    let risks = path_risks(&repository_root, &candidate_paths);
    let remote_configured = successful_text(&git(&repository_root, ["remote"])?).is_some();

    Ok(RepositoryInspection {
        requested_path,
        repository_root: Some(repository_root),
        git_initialized: true,
        head_sha,
        current_branch,
        dirty_paths,
        risk_warnings: risks.warnings,
        blocking_secret_paths: risks.secrets,
        remote_configured,
    })
}

pub(crate) fn initialize_git(path: &Path) -> Result<(), LauncherError> {
    let output = git(path, ["init", "--quiet"])?;
    require_git_success(&output, "repository.git-init-failed", "Git 初始化失败")?;
    Ok(())
}

pub(crate) fn create_snapshot(
    repository_root: &Path,
    head_sha: Option<&str>,
    temp_root: &Path,
) -> Result<SnapshotBaseline, LauncherError> {
    fs::create_dir_all(temp_root).map_err(|error| {
        LauncherError::product(
            "product.snapshot-temp-unavailable",
            format!("无法创建临时索引目录 {}：{error}", temp_root.display()),
        )
    })?;
    let tree = write_snapshot_tree(repository_root, head_sha, temp_root)?;
    let reference = format!("refs/winwincode/snapshots/{tree}");
    if let Some(existing) = existing_snapshot(repository_root, &reference, &tree)? {
        return Ok(SnapshotBaseline {
            commit_sha: existing,
            reference,
        });
    }
    let commit = create_snapshot_commit(repository_root, &tree, head_sha)?;
    let zero = "0".repeat(commit.len());
    let output = git(repository_root, ["update-ref", &reference, &commit, &zero])?;
    require_git_success(
        &output,
        "repository.snapshot-ref-failed",
        "无法创建 Snapshot 引用",
    )?;
    Ok(SnapshotBaseline {
        commit_sha: commit,
        reference,
    })
}

fn write_snapshot_tree(
    repository_root: &Path,
    head_sha: Option<&str>,
    temp_root: &Path,
) -> Result<String, LauncherError> {
    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let index_path = temp_root.join(format!("snapshot-{}-{sequence}.index", std::process::id()));
    let _cleanup = TemporaryIndex(index_path.clone());
    let index_environment = [("GIT_INDEX_FILE", index_path.as_os_str())];
    let read_tree_arguments = head_sha.map_or_else(
        || vec!["read-tree", "--empty"],
        |head| vec!["read-tree", head],
    );
    let output = git_with(
        repository_root,
        read_tree_arguments,
        &index_environment,
        None,
    )?;
    require_git_success(
        &output,
        "repository.snapshot-index-failed",
        "无法准备 Snapshot 临时索引",
    )?;
    let output = git_with(
        repository_root,
        ["add", "-A", "--", "."],
        &index_environment,
        None,
    )?;
    require_git_success(
        &output,
        "repository.snapshot-add-failed",
        "无法把已确认的工作区内容写入 Snapshot 临时索引",
    )?;
    let output = git_with(repository_root, ["write-tree"], &index_environment, None)?;
    require_text(
        &output,
        "repository.snapshot-tree-failed",
        "无法生成 Snapshot tree",
    )
}

fn existing_snapshot(
    repository_root: &Path,
    reference: &str,
    expected_tree: &str,
) -> Result<Option<String>, LauncherError> {
    let output = git(
        repository_root,
        ["rev-parse", "--verify", &format!("{reference}^{{commit}}")],
    )?;
    let Some(existing) = successful_text(&output) else {
        return Ok(None);
    };
    let output = git(repository_root, ["show", "-s", "--format=%T", &existing])?;
    let existing_tree = require_text(
        &output,
        "repository.snapshot-ref-invalid",
        "现有 Snapshot 引用不可读取",
    )?;
    if existing_tree == expected_tree {
        Ok(Some(existing))
    } else {
        Err(LauncherError::repository(
            "repository.snapshot-ref-conflict",
            format!("Snapshot 引用 {reference} 已指向其他内容。"),
        ))
    }
}

fn create_snapshot_commit(
    repository_root: &Path,
    tree: &str,
    head_sha: Option<&str>,
) -> Result<String, LauncherError> {
    let mut arguments = vec!["commit-tree", tree];
    if let Some(head) = head_sha {
        arguments.extend(["-p", head]);
    }
    let environment = [
        (
            "GIT_AUTHOR_NAME",
            std::ffi::OsStr::new("WinWinCode Snapshot"),
        ),
        (
            "GIT_AUTHOR_EMAIL",
            std::ffi::OsStr::new("snapshot@winwincode.invalid"),
        ),
        (
            "GIT_AUTHOR_DATE",
            std::ffi::OsStr::new("2000-01-01T00:00:00Z"),
        ),
        (
            "GIT_COMMITTER_NAME",
            std::ffi::OsStr::new("WinWinCode Snapshot"),
        ),
        (
            "GIT_COMMITTER_EMAIL",
            std::ffi::OsStr::new("snapshot@winwincode.invalid"),
        ),
        (
            "GIT_COMMITTER_DATE",
            std::ffi::OsStr::new("2000-01-01T00:00:00Z"),
        ),
    ];
    let output = git_with(
        repository_root,
        arguments,
        &environment,
        Some(b"WinWinCode baseline snapshot\n"),
    )?;
    require_text(
        &output,
        "repository.snapshot-commit-failed",
        "无法创建 Snapshot commit",
    )
}

pub(crate) fn command_version(program: &str) -> Result<String, LauncherError> {
    let output = Command::new(program)
        .arg("--version")
        .output()
        .map_err(|error| {
            LauncherError::environment(
                "environment.command-unavailable",
                format!("{program} 不可执行：{error}"),
            )
        })?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    } else {
        Err(LauncherError::environment(
            "environment.command-failed",
            format!(
                "{program} --version 执行失败：{}",
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        ))
    }
}

pub(crate) fn disk_available_kib(path: &Path) -> Option<u64> {
    let output = Command::new("df").args(["-Pk"]).arg(path).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let last = text.lines().rfind(|line| !line.trim().is_empty())?;
    last.split_whitespace().nth(3)?.parse().ok()
}

pub(crate) fn total_memory_bytes() -> Option<u64> {
    if let Ok(meminfo) = fs::read_to_string("/proc/meminfo")
        && let Some(kib) = meminfo
            .lines()
            .find_map(|line| line.strip_prefix("MemTotal:"))
            .and_then(|value| value.split_whitespace().next())
            .and_then(|value| value.parse::<u64>().ok())
    {
        return kib.checked_mul(1024);
    }
    let output = Command::new("sysctl")
        .args(["-n", "hw.memsize"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()?.trim().parse().ok()
}

pub(crate) fn container_runtime() -> Option<String> {
    for runtime in ["docker", "podman"] {
        if let Ok(version) = command_version(runtime) {
            return Some(version);
        }
    }
    None
}

fn dirty_paths(repository_root: &Path) -> Result<Vec<String>, LauncherError> {
    let mut paths = BTreeSet::new();
    for arguments in [
        vec!["diff", "--name-only", "-z"],
        vec!["diff", "--cached", "--name-only", "-z"],
        vec!["ls-files", "--others", "--exclude-standard", "-z"],
    ] {
        let output = git(repository_root, arguments)?;
        require_git_success(
            &output,
            "repository.git-status-failed",
            "无法读取工作区变化",
        )?;
        for path in output.stdout.split(|byte| *byte == 0) {
            if !path.is_empty() {
                paths.insert(String::from_utf8_lossy(path).into_owned());
            }
        }
    }
    Ok(paths.into_iter().collect())
}

fn tracked_paths(repository_root: &Path) -> Result<Vec<String>, LauncherError> {
    let output = git(repository_root, ["ls-files", "-z"])?;
    require_git_success(
        &output,
        "repository.git-files-failed",
        "无法读取 Git 跟踪文件",
    )?;
    Ok(output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(|path| String::from_utf8_lossy(path).into_owned())
        .collect())
}

fn worktree_paths_without_git(root: &Path) -> Result<Vec<String>, LauncherError> {
    let mut paths = Vec::new();
    visit_files(root, root, &mut paths)?;
    paths.sort();
    Ok(paths)
}

fn visit_files(
    root: &Path,
    directory: &Path,
    paths: &mut Vec<String>,
) -> Result<(), LauncherError> {
    if paths.len() >= MAX_PRE_INIT_FILES {
        return Err(LauncherError::repository(
            "repository.pre-init-scan-limit",
            format!(
                "Git 初始化前发现超过 {MAX_PRE_INIT_FILES} 个文件。请先移出或忽略依赖、构建产物和大型目录。"
            ),
        ));
    }
    let entries = fs::read_dir(directory).map_err(|error| {
        LauncherError::repository(
            "repository.directory-read-failed",
            format!("无法读取目录 {}：{error}", directory.display()),
        )
    })?;
    for entry in entries {
        if paths.len() >= MAX_PRE_INIT_FILES {
            return Err(LauncherError::repository(
                "repository.pre-init-scan-limit",
                format!(
                    "Git 初始化前发现超过 {MAX_PRE_INIT_FILES} 个文件。请先移出或忽略依赖、构建产物和大型目录。"
                ),
            ));
        }
        let entry = entry.map_err(|error| {
            LauncherError::repository(
                "repository.directory-read-failed",
                format!("无法读取目录项：{error}"),
            )
        })?;
        if entry.file_name() == ".git" {
            continue;
        }
        let file_type = entry.file_type().map_err(|error| {
            LauncherError::repository(
                "repository.file-type-failed",
                format!("无法检查 {}：{error}", entry.path().display()),
            )
        })?;
        if file_type.is_dir() {
            visit_files(root, &entry.path(), paths)?;
        } else if file_type.is_file() || file_type.is_symlink() {
            let entry_path = entry.path();
            let relative = entry_path.strip_prefix(root).map_err(|error| {
                LauncherError::repository(
                    "repository.path-invalid",
                    format!("目录路径无法归一化：{error}"),
                )
            })?;
            paths.push(relative.to_string_lossy().replace('\\', "/"));
        }
    }
    Ok(())
}

struct PathRisks {
    warnings: Vec<String>,
    secrets: Vec<String>,
}

fn path_risks(root: &Path, paths: &[String]) -> PathRisks {
    let mut warnings = BTreeSet::new();
    let mut secrets = BTreeSet::new();
    for path in paths {
        if is_secret_path(path) {
            secrets.insert(path.clone());
            warnings.insert(format!("疑似秘密文件不会进入 Snapshot：{path}"));
        }
        if has_segment(path, &["node_modules", "target", "dist", "build", ".cache"]) {
            warnings.insert(format!("检测到构建或依赖产物：{path}"));
        }
        if fs::metadata(root.join(path))
            .ok()
            .is_some_and(|metadata| metadata.len() > LARGE_FILE_BYTES)
        {
            warnings.insert(format!("检测到大于 10 MiB 的文件：{path}"));
        }
    }
    PathRisks {
        warnings: warnings.into_iter().collect(),
        secrets: secrets.into_iter().collect(),
    }
}

fn is_secret_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    let name = Path::new(&lower)
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or(&lower);
    let env_secret = (name == ".env" || name.starts_with(".env."))
        && !name.ends_with(".example")
        && !name.ends_with(".sample")
        && !name.ends_with(".template");
    env_secret
        || name == ".npmrc"
        || name == "id_rsa"
        || name == "id_ed25519"
        || name == "credentials.json"
        || name.starts_with("secrets.")
        || ["pem", "key", "p12", "pfx"]
            .iter()
            .any(|extension| name.ends_with(&format!(".{extension}")))
}

fn has_segment(path: &str, expected: &[&str]) -> bool {
    path.split('/').any(|segment| {
        expected
            .iter()
            .any(|candidate| segment.eq_ignore_ascii_case(candidate))
    })
}

fn successful_text(output: &Output) -> Option<String> {
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn require_text(
    output: &Output,
    code: &'static str,
    message: &str,
) -> Result<String, LauncherError> {
    require_git_success(output, code, message)?;
    let text = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if text.is_empty() {
        Err(LauncherError::repository(
            code,
            format!("{message}：Git 没有返回对象 ID。"),
        ))
    } else {
        Ok(text)
    }
}

fn require_git_success(
    output: &Output,
    code: &'static str,
    message: &str,
) -> Result<(), LauncherError> {
    if output.status.success() {
        Ok(())
    } else {
        Err(LauncherError::repository(
            code,
            format!(
                "{message}：{}",
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        ))
    }
}

fn git<I, S>(root: &Path, arguments: I) -> Result<Output, LauncherError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    git_with(root, arguments, &[], None)
}

fn git_with<I, S>(
    root: &Path,
    arguments: I,
    environment: &[(&str, &std::ffi::OsStr)],
    stdin: Option<&[u8]>,
) -> Result<Output, LauncherError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let mut command = Command::new("git");
    command.arg("-C").arg(root).args(arguments);
    for (name, value) in environment {
        command.env(name, value);
    }
    if stdin.is_some() {
        command.stdin(Stdio::piped());
    }
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command.spawn().map_err(|error| {
        LauncherError::environment(
            "environment.git-unavailable",
            format!("Git 不可执行：{error}"),
        )
    })?;
    if let Some(input) = stdin {
        child
            .stdin
            .take()
            .ok_or_else(|| {
                LauncherError::product(
                    "product.git-stdin-unavailable",
                    "无法向 Git 写入 Snapshot 说明。",
                )
            })?
            .write_all(input)
            .map_err(|error| {
                LauncherError::product(
                    "product.git-stdin-failed",
                    format!("无法向 Git 写入 Snapshot 说明：{error}"),
                )
            })?;
    }
    child.wait_with_output().map_err(|error| {
        LauncherError::environment("environment.git-failed", format!("Git 执行失败：{error}"))
    })
}

struct TemporaryIndex(PathBuf);

impl Drop for TemporaryIndex {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
        let lock = self.0.with_extension("index.lock");
        let _ = fs::remove_file(lock);
    }
}
