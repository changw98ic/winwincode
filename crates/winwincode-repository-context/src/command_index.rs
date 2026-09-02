use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

use crate::{LocalCodeIndexMode, LocalCodeIndexPort, LocalCodeIndexProbe, RepositoryContextError};

#[derive(Clone, Debug)]
pub struct CommandLocalCodeIndex {
    program: OsString,
    script: PathBuf,
    runtime_root: Option<PathBuf>,
}

impl CommandLocalCodeIndex {
    pub fn new(script: impl Into<PathBuf>) -> Self {
        Self {
            program: OsString::from("node"),
            script: script.into(),
            runtime_root: None,
        }
    }

    #[must_use]
    pub fn with_program(mut self, program: impl Into<OsString>) -> Self {
        self.program = program.into();
        self
    }

    #[must_use]
    pub fn with_runtime_root(mut self, runtime_root: impl Into<PathBuf>) -> Self {
        self.runtime_root = Some(runtime_root.into());
        self
    }

    fn run(
        &self,
        operation: &str,
        repository_root: &Path,
    ) -> Result<Value, RepositoryContextError> {
        if !self.script.is_file() {
            return Err(RepositoryContextError::IndexCommand(format!(
                "configured script does not exist: {}",
                self.script.display()
            )));
        }
        let mut command = Command::new(&self.program);
        command
            .arg(&self.script)
            .arg(operation)
            .arg(repository_root);
        if let Some(runtime_root) = &self.runtime_root {
            command.env("CPB_ROOT", runtime_root);
        }
        let output = command
            .output()
            .map_err(|error| RepositoryContextError::IndexCommand(error.to_string()))?;
        if !output.status.success() {
            return Err(RepositoryContextError::IndexCommand(
                String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            ));
        }
        serde_json::from_slice(&output.stdout)
            .map_err(|error| RepositoryContextError::IndexResponse(error.to_string()))
    }
}

impl LocalCodeIndexPort for CommandLocalCodeIndex {
    fn status(
        &self,
        repository_root: &Path,
        _baseline_sha: &str,
    ) -> Result<LocalCodeIndexProbe, RepositoryContextError> {
        let value = self.run("status", repository_root)?;
        parse_status(&value)
    }

    fn refresh(
        &self,
        repository_root: &Path,
        _baseline_sha: &str,
    ) -> Result<(), RepositoryContextError> {
        self.run("check", repository_root).map(|_| ())
    }
}

fn parse_status(value: &Value) -> Result<LocalCodeIndexProbe, RepositoryContextError> {
    let available = value
        .get("available")
        .and_then(Value::as_bool)
        .ok_or_else(|| RepositoryContextError::IndexResponse("missing available boolean".into()))?;
    let fresh = value
        .get("fresh")
        .and_then(Value::as_bool)
        .ok_or_else(|| RepositoryContextError::IndexResponse("missing fresh boolean".into()))?;
    let mode = match value.get("mode").and_then(Value::as_str) {
        Some("ast-grep-outline") => LocalCodeIndexMode::AstGrepOutline,
        Some("git-file-inventory") | None => LocalCodeIndexMode::GitFileInventory,
        Some(other) => {
            return Err(RepositoryContextError::IndexResponse(format!(
                "unsupported mode: {other}"
            )));
        }
    };
    let baseline_sha = find_baseline_sha(value).map(str::to_ascii_lowercase);
    let detail = value
        .get("detail")
        .and_then(Value::as_str)
        .unwrap_or("local code-index status command")
        .to_owned();
    Ok(LocalCodeIndexProbe {
        available,
        fresh,
        mode,
        baseline_sha,
        detail,
    })
}

fn find_baseline_sha(value: &Value) -> Option<&str> {
    value
        .get("baselineSha")
        .and_then(Value::as_str)
        .or_else(|| value.get("gitHead").and_then(Value::as_str))
        .or_else(|| {
            value
                .get("fingerprint")
                .and_then(|fingerprint| fingerprint.get("gitHead"))
                .and_then(Value::as_str)
        })
        .or_else(|| {
            value
                .get("sourceFingerprint")
                .and_then(|fingerprint| fingerprint.get("gitHead"))
                .and_then(Value::as_str)
        })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn parses_nested_fingerprint_and_explicit_mode() {
        let probe = parse_status(&json!({
            "available": true,
            "fresh": true,
            "mode": "ast-grep-outline",
            "fingerprint": { "gitHead": "ABC123" }
        }))
        .expect("status should parse");

        assert_eq!(probe.mode, LocalCodeIndexMode::AstGrepOutline);
        assert_eq!(probe.baseline_sha.as_deref(), Some("abc123"));
    }

    #[test]
    fn rejects_unknown_coverage_mode() {
        let error = parse_status(&json!({
            "available": true,
            "fresh": true,
            "mode": "complete-call-graph"
        }))
        .expect_err("unknown coverage must not be trusted");

        assert!(error.to_string().contains("unsupported mode"));
    }
}
