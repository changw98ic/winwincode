// SPDX-License-Identifier: Apache-2.0

//! Contract for the Device-owned managed application lifecycle.
//!
//! This contract is deliberately separate from the WorkerSession contract.
//! A managed application is a candidate/live HTTP process with its own run
//! identity and process group; a Worker is never a substitute for it.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

pub const MANAGED_APP_RUN_CONFIG_SCHEMA_VERSION: &str = "winwincode/managed-app-run-v1";
pub const MANAGED_APP_RUN_TEMPLATE_SCHEMA_VERSION: &str = "winwincode/managed-app-template-v1";

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ManagedAppMode {
    Live,
    FrozenCandidate,
}

#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedAppHealthCheck {
    pub path: String,
    #[serde(default = "default_health_timeout_ms")]
    #[serde(rename = "timeoutMs")]
    pub timeout_ms: u32,
}

fn default_health_timeout_ms() -> u32 {
    5_000
}

#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedAppRunConfig {
    #[serde(rename = "schemaVersion")]
    pub schema_version: String,
    #[serde(rename = "runId")]
    pub run_id: String,
    #[serde(rename = "repositoryBindingId")]
    pub repository_binding_id: String,
    /// Revision of the repository-scoped template captured when this run
    /// was confirmed. It makes the launch settings used by a WorkRun
    /// restart-stable and auditable after the template changes.
    #[serde(rename = "templateRevision")]
    pub template_revision: u64,
    pub attempt: u32,
    pub mode: ManagedAppMode,
    /// Candidate identity is mandatory for frozen previews and forbidden for
    /// live previews. The Device resolves the repository binding locally.
    #[serde(rename = "candidateCommit")]
    pub candidate_commit: Option<String>,
    /// Repository-relative working directory; absolute paths never cross the
    /// Server→Device contract.
    pub cwd: String,
    /// Executable plus arguments. The Device validates this against its local
    /// allowlist before spawning.
    pub argv: Vec<String>,
    /// The execution environment is intentionally empty. Device-local
    /// process policy owns any future trusted variables.
    pub env: BTreeMap<String, String>,
    #[serde(rename = "healthCheck")]
    pub health_check: ManagedAppHealthCheck,
    #[serde(rename = "listenPort")]
    pub listen_port: u16,
    #[serde(rename = "sourceId")]
    pub source_id: String,
}

/// Repository-scoped launch settings. Server materializes a full
/// [`ManagedAppRunConfig`] from this template at WorkRun confirmation time.
#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedAppRunTemplate {
    #[serde(rename = "schemaVersion")]
    pub schema_version: String,
    pub mode: ManagedAppMode,
    pub cwd: String,
    pub argv: Vec<String>,
    #[serde(rename = "healthCheck")]
    pub health_check: ManagedAppHealthCheck,
    #[serde(rename = "listenPort")]
    pub listen_port: u16,
}

impl ManagedAppRunTemplate {
    /// Validates repository-scoped launch settings before persistence.
    ///
    /// # Errors
    ///
    /// Returns the same portable-field errors as a materialized run config.
    pub fn validate(&self) -> Result<(), ManagedAppContractError> {
        if self.schema_version != MANAGED_APP_RUN_TEMPLATE_SCHEMA_VERSION
            || self.argv.is_empty()
            || self.argv.iter().any(String::is_empty)
            || self.cwd.is_empty()
            || self.cwd.starts_with('/')
            || self
                .cwd
                .split('/')
                .any(|part| part == ".." || part.is_empty())
            || !self.health_check.path.starts_with('/')
            || self.health_check.path.contains("..")
            || self.health_check.timeout_ms == 0
            || self.listen_port == 0
        {
            return Err(ManagedAppContractError::Invalid("template"));
        }
        Ok(())
    }
}

impl ManagedAppRunConfig {
    /// Validates the portable Server-to-Device run configuration.
    ///
    /// # Errors
    ///
    /// Returns [`ManagedAppContractError`] when an identity, path, command,
    /// health check, environment key, or candidate mode is invalid.
    pub fn validate(&self) -> Result<(), ManagedAppContractError> {
        if self.schema_version != MANAGED_APP_RUN_CONFIG_SCHEMA_VERSION {
            return Err(ManagedAppContractError::Invalid("schemaVersion"));
        }
        for (field, value) in [
            ("runId", self.run_id.as_str()),
            ("repositoryBindingId", self.repository_binding_id.as_str()),
            ("sourceId", self.source_id.as_str()),
        ] {
            if !portable_identifier(value) {
                return Err(ManagedAppContractError::Invalid(field));
            }
        }
        if self.attempt == 0
            || self.template_revision == 0
            || self.argv.is_empty()
            || self.argv.iter().any(String::is_empty)
        {
            return Err(ManagedAppContractError::Invalid("argv/attempt"));
        }
        if self.cwd.is_empty()
            || self.cwd.starts_with('/')
            || self
                .cwd
                .split('/')
                .any(|part| part == ".." || part.is_empty())
        {
            return Err(ManagedAppContractError::Invalid("cwd"));
        }
        if !self.health_check.path.starts_with('/')
            || self.health_check.path.contains("..")
            || self.health_check.timeout_ms == 0
            || self.listen_port == 0
        {
            return Err(ManagedAppContractError::Invalid("healthCheck"));
        }
        match (self.mode, self.candidate_commit.as_deref()) {
            (ManagedAppMode::Live, None) => {}
            (ManagedAppMode::FrozenCandidate, Some(commit)) if is_git_commit(commit) => {}
            _ => return Err(ManagedAppContractError::Invalid("candidateCommit")),
        }
        if !self.env.is_empty() {
            return Err(ManagedAppContractError::Invalid("env"));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ManagedAppOperation {
    Start,
    Stop,
    Restart,
    Query,
}

#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedAppCommand {
    #[serde(rename = "schemaVersion")]
    pub schema_version: String,
    pub operation: ManagedAppOperation,
    #[serde(rename = "idempotencyKey")]
    pub idempotency_key: String,
    #[serde(rename = "occupancyLeaseId")]
    pub occupancy_lease_id: String,
    #[serde(rename = "occupancyFencingToken")]
    pub occupancy_fencing_token: u64,
    pub config: Option<ManagedAppRunConfig>,
    #[serde(rename = "runId")]
    pub run_id: String,
}

impl ManagedAppCommand {
    /// Validates the managed application command and its optional run config.
    ///
    /// # Errors
    ///
    /// Returns [`ManagedAppContractError`] when command authority is invalid
    /// or the operation carries an incompatible run configuration.
    pub fn validate(&self) -> Result<(), ManagedAppContractError> {
        if self.schema_version != MANAGED_APP_RUN_CONFIG_SCHEMA_VERSION
            || !portable_identifier(&self.idempotency_key)
            || !portable_identifier(&self.occupancy_lease_id)
            || self.occupancy_fencing_token == 0
            || !portable_identifier(&self.run_id)
        {
            return Err(ManagedAppContractError::Invalid("command identity"));
        }
        match (self.operation, self.config.as_ref()) {
            (ManagedAppOperation::Start | ManagedAppOperation::Restart, Some(config))
                if config.run_id == self.run_id =>
            {
                config.validate()
            }
            (ManagedAppOperation::Stop | ManagedAppOperation::Query, None) => Ok(()),
            _ => Err(ManagedAppContractError::Invalid("config")),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ManagedAppState {
    Starting,
    Healthy,
    Unhealthy,
    Stopped,
    Exited,
    Missing,
}

#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedAppStatus {
    #[serde(rename = "schemaVersion")]
    pub schema_version: String,
    #[serde(rename = "runId")]
    pub run_id: String,
    pub state: ManagedAppState,
    pub pid: Option<u32>,
    #[serde(rename = "processStartIdentity")]
    pub process_start_identity: Option<String>,
    #[serde(rename = "exitCode")]
    pub exit_code: Option<i32>,
    #[serde(rename = "sourceId")]
    pub source_id: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ManagedAppContractError {
    Invalid(&'static str),
}

impl std::fmt::Display for ManagedAppContractError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Self::Invalid(field) = self;
        write!(f, "invalid managed app field {field}")
    }
}

impl std::error::Error for ManagedAppContractError {}

fn portable_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 200
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'/' | b'@' | b'-')
        })
}

fn is_git_commit(value: &str) -> bool {
    matches!(value.len(), 40 | 64)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(mode: ManagedAppMode, commit: Option<String>) -> ManagedAppRunConfig {
        ManagedAppRunConfig {
            schema_version: MANAGED_APP_RUN_CONFIG_SCHEMA_VERSION.to_owned(),
            run_id: "run_demo".to_owned(),
            repository_binding_id: "rbd_demo".to_owned(),
            template_revision: 1,
            attempt: 1,
            mode,
            candidate_commit: commit,
            cwd: ".".to_owned(),
            argv: vec!["npm".to_owned(), "run".to_owned(), "dev".to_owned()],
            env: BTreeMap::new(),
            health_check: ManagedAppHealthCheck {
                path: "/health".to_owned(),
                timeout_ms: 1000,
            },
            listen_port: 3000,
            source_id: "pvs_demo".to_owned(),
        }
    }

    #[test]
    fn live_and_frozen_identities_cannot_be_mixed() {
        assert!(
            config(ManagedAppMode::Live, Some("a".repeat(40)))
                .validate()
                .is_err()
        );
        assert!(
            config(ManagedAppMode::FrozenCandidate, None)
                .validate()
                .is_err()
        );
        assert!(config(ManagedAppMode::Live, None).validate().is_ok());
        assert!(
            config(ManagedAppMode::FrozenCandidate, Some("a".repeat(40)))
                .validate()
                .is_ok()
        );
    }

    #[test]
    fn absolute_cwd_and_any_env_values_are_rejected() {
        let mut run = config(ManagedAppMode::Live, None);
        run.cwd = "/tmp".to_owned();
        assert!(run.validate().is_err());
        let mut run = config(ManagedAppMode::Live, None);
        run.env.insert("API_KEY".to_owned(), "secret".to_owned());
        assert!(run.validate().is_err());
    }

    #[test]
    fn repository_template_does_not_accept_environment_values() {
        let template = serde_json::json!({
            "schemaVersion": MANAGED_APP_RUN_TEMPLATE_SCHEMA_VERSION,
            "mode": "live",
            "cwd": ".",
            "argv": ["pnpm", "dev"],
            "env": {"API_KEY": "secret"},
            "healthCheck": {"path": "/health", "timeoutMs": 1000},
            "listenPort": 3000
        });
        assert!(serde_json::from_value::<ManagedAppRunTemplate>(template).is_err());
    }
}
