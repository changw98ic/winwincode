// SPDX-License-Identifier: Apache-2.0
//! Trusted dependency adapter for the bundled offline public-example runner.
use crate::tool_input_source::source_digest;
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::Command,
};
use winwincode_kernel::{
    ToolCoalescingPermission, ToolDependencySnapshot, ToolInputProof, ToolInputProofRequest,
    ToolReusePermission,
};
const SCRIPT: &[u8] = include_bytes!("../../../scripts/benchmark-public-smoke.py");
const REVISION: &str = "fa9da301e493fb88d48c86cb8954ed46d9cd2ffe";
#[derive(Clone)]
pub(crate) struct PublicSmokeAdapter {
    workspace: PathBuf,
    task_id: String,
    image: String,
    evidence: PathBuf,
    suffixes: Vec<String>,
    entry: String,
    configuration_digest: String,
    frozen_files: BTreeMap<PathBuf, String>,
}
#[derive(Deserialize)]
struct Extension {
    kind: String,
    id: String,
    data: String,
    projection: String,
}
impl PublicSmokeAdapter {
    pub(crate) fn load(home: &Path, workspace: &Path) -> BTreeMap<String, Self> {
        let Some(bytes) = bounded(&home.join("device-extensions.json"), 4 * 1024 * 1024) else {
            return BTreeMap::new();
        };
        let Ok(entries) = serde_json::from_slice::<Vec<Extension>>(&bytes) else {
            return BTreeMap::new();
        };
        entries
            .into_iter()
            .filter_map(|entry| {
                if entry.kind != "mcp" || !entry.id.starts_with("benchmark_public_smoke_") {
                    return None;
                }
                let projection: Value = serde_json::from_str(&entry.projection).ok()?;
                if projection["enabled"] != true
                    || projection["connectionStatus"] != "ready"
                    || !projection["toolNames"]
                        .as_array()?
                        .iter()
                        .any(|name| name == "public_smoke")
                {
                    return None;
                }
                Some((entry.id, Self::recognize(&entry.data, workspace)?))
            })
            .collect()
    }
    fn recognize(configuration: &str, workspace: &Path) -> Option<Self> {
        let (script, options) = configuration_options(configuration)?;
        let task_root = fs::canonicalize(options.get("--task-root")?).ok()?;
        let task_id = options.get("--task-id")?.clone();
        if task_id.is_empty()
            || !task_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
        {
            return None;
        }
        let image = options.get("--image-id")?.clone();
        if image.len() != 71
            || !image.starts_with("sha256:")
            || !image[7..]
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        {
            return None;
        }
        let evidence_path = PathBuf::from(options.get("--evidence-directory")?);
        if !evidence_path.is_absolute() {
            return None;
        }
        fs::create_dir_all(&evidence_path).ok()?;
        let evidence = fs::canonicalize(evidence_path).ok()?;
        let workspace = fs::canonicalize(workspace).ok()?;
        if [&script, &task_root, &evidence]
            .iter()
            .any(|path| path.starts_with(&workspace))
        {
            return None;
        }
        let revision = Command::new("git")
            .arg("-C")
            .arg(&task_root)
            .args(["rev-parse", "HEAD"])
            .output()
            .ok()?;
        if !revision.status.success()
            || std::str::from_utf8(&revision.stdout).ok()?.trim() != REVISION
        {
            return None;
        }
        let catalog: Value =
            serde_json::from_slice(&bounded(&task_root.join("catalog.json"), 1024 * 1024)?).ok()?;
        let task = catalog
            .as_array()?
            .iter()
            .find(|task| task["id"] == task_id)?;
        let manifest: Value = serde_json::from_slice(&bounded(
            &task_root.join("environments/manifest.json"),
            1024 * 1024,
        )?)
        .ok()?;
        let environment = &manifest[task["environment"].as_str()?];
        let suffixes = environment["suffixes"]
            .as_array()?
            .iter()
            .map(|v| v.as_str().map(str::to_owned))
            .collect::<Option<Vec<_>>>()?;
        let entry = environment["entry"].as_str()?.to_owned();
        let mut frozen_files = BTreeMap::new();
        for path in [
            script,
            task_root.join("catalog.json"),
            task_root.join("environments/manifest.json"),
            task_root.join("tools/sandbox.py"),
            task_root.join("tasks").join(&task_id).join("examples.json"),
        ] {
            frozen_files.insert(path.clone(), hash(&bounded(&path, 2 * 1024 * 1024)?));
        }
        Some(Self {
            task_id,
            image,
            evidence,
            workspace,
            suffixes,
            entry,
            configuration_digest: hash(configuration.as_bytes()),
            frozen_files,
        })
    }
    pub(crate) fn snapshot(
        &self,
        policy: &str,
        account: &str,
        session: &str,
        validity: &str,
    ) -> Option<ToolDependencySnapshot> {
        if !self.policy_valid() || !self.workspace.join(&self.entry).is_file() {
            return None;
        }
        Some(ToolDependencySnapshot {
            policy_revision: hash(
                &serde_json::to_vec(&(policy, &self.configuration_digest, &self.frozen_files))
                    .ok()?,
            ),
            dependency_digest: source_digest(&self.workspace, &self.suffixes).ok()?,
            account_scope_digest: hash(account.as_bytes()),
            session_scope_digest: hash(session.as_bytes()),
            validity_epoch: hash(validity.as_bytes()),
            reuse: ToolReusePermission::ImmutableValue,
            coalescing: ToolCoalescingPermission::SharedRead,
        })
    }
    pub(crate) fn verify(&self, request: &ToolInputProofRequest) -> Option<ToolInputProof> {
        if !self.policy_valid() {
            return None;
        }
        let report = request.output.get("structuredContent")?;
        let attempt = report["attemptId"].as_str()?;
        if attempt.len() != 32 || !attempt.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return None;
        }
        let directory = self.evidence.join(attempt);
        if !fs::symlink_metadata(&directory).ok()?.is_dir() {
            return None;
        }
        let retained: Value =
            serde_json::from_slice(&bounded(&directory.join("result.json"), 128 * 1024)?).ok()?;
        if &retained != report
            || report["status"] != "evaluated"
            || report["taskId"] != self.task_id
            || report["taskRevision"] != REVISION
            || report["imageId"] != self.image
            || report["platform"] != "linux/arm64"
            || report["formalBenchmark"] != false
            || report["gradeScope"] != "public_examples"
        {
            return None;
        }
        let input_digest = source_digest(&directory.join("source"), &self.suffixes).ok()?;
        if report["source"]["sha256"] != input_digest {
            return None;
        }
        let stdout = bounded(&directory.join("stdout.bin"), 4 * 1024 * 1024)?;
        let stderr = bounded(&directory.join("stderr.bin"), 4 * 1024 * 1024)?;
        let information = json!([
            self.task_id,
            self.image,
            input_digest,
            report["returncode"],
            report["reason"],
            report["publicResult"],
            hash(&stdout),
            hash(&stderr)
        ]);
        Some(ToolInputProof {
            input_digest,
            evidence_digest: hash(&serde_json::to_vec(&information).ok()?),
        })
    }
    fn policy_valid(&self) -> bool {
        self.frozen_files.iter().all(|(path, expected)| {
            bounded(path, 2 * 1024 * 1024).is_some_and(|bytes| hash(&bytes) == *expected)
        })
    }
}
fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn bounded(path: &Path, limit: u64) -> Option<Vec<u8>> {
    crate::tool_input_source::read_source_file(path, limit).ok()
}

#[cfg(test)]
#[path = "public_smoke_adapter_tests.rs"]
mod tests;

fn configuration_options(configuration: &str) -> Option<(PathBuf, BTreeMap<String, String>)> {
    let config: Value = serde_json::from_str(configuration).ok()?;
    if config["command"] != "/usr/bin/python3" {
        return None;
    }
    let args = config["args"]
        .as_array()?
        .iter()
        .map(Value::as_str)
        .collect::<Option<Vec<_>>>()?;
    if !matches!(args.len(), 10 | 12) || args[0] != "-I" {
        return None;
    }
    let script = fs::canonicalize(args[1]).ok()?;
    if bounded(&script, 128 * 1024)?.as_slice() != SCRIPT {
        return None;
    }
    let mut options = BTreeMap::new();
    for pair in args[2..].chunks_exact(2) {
        if options
            .insert(pair[0].to_owned(), pair[1].to_owned())
            .is_some()
        {
            return None;
        }
    }
    if options.keys().any(|key| {
        ![
            "--task-root",
            "--task-id",
            "--image-id",
            "--evidence-directory",
            "--execution-lock",
        ]
        .contains(&key.as_str())
    }) {
        return None;
    }
    Some((script, options))
}
