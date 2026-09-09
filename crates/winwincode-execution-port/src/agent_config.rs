// SPDX-License-Identifier: Apache-2.0

//! Stable Agent identity and immutable per-session profile snapshots.

use std::fmt;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use winwincode_domain::{Sha256Digest, WorkerId};

use crate::generated::WorkerCapabilitySet;

/// Version of the Worker-owned session configuration snapshot.
pub const AGENT_SESSION_CONFIG_SCHEMA_VERSION: u64 = 1;

/// Agent identity that remains stable across process and Session restarts.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct AgentIdentity {
    /// Stable identity derived from the Worker and role.
    pub id: String,
    /// Stable Worker identity that owns this Agent.
    pub worker_id: WorkerId,
    /// Human-readable role name.
    pub name: String,
    /// Execution role inherited from the dispatched job.
    pub role: String,
    /// Exact capabilities registered by the Worker.
    pub capabilities: WorkerCapabilitySet,
}

/// Effective model, tool, sandbox, and instruction settings for one profile.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct AgentProfileSettings {
    /// Provider selected by the embedded runtime.
    pub provider: String,
    /// Model selected within the provider.
    pub model: String,
    /// Effective reasoning setting or its explicit provider-default marker.
    pub reasoning: String,
    /// Sorted effective tool capability identifiers.
    pub tools: Vec<String>,
    /// Effective sandbox or workspace mode.
    pub sandbox: String,
    /// Effective developer instructions; absent when the Session has none.
    pub instructions: Option<String>,
}

/// Explainable inputs of one resolved profile revision.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct AgentProfileSource {
    /// Execution profile selected by the dispatched job.
    pub execution_profile: String,
    /// Capability revision registered by the Worker.
    pub worker_capability_digest: Sha256Digest,
    /// Exact effective settings used to start the Session.
    pub settings: AgentProfileSettings,
}

/// Immutable profile revision copied into a newly accepted Session.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct AgentProfile {
    /// Content-derived revision of the profile source.
    pub revision: Sha256Digest,
    /// Inputs that explain this revision.
    pub source: AgentProfileSource,
}

/// Reproducible configuration inherited by one `WorkerSession`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct AgentSessionConfigSnapshot {
    /// Snapshot schema version.
    pub schema_version: u64,
    /// Stable Agent identity inherited by the Session.
    pub identity: AgentIdentity,
    /// Immutable profile revision inherited by the Session.
    pub profile: AgentProfile,
    /// Digest of the complete Session configuration.
    pub snapshot_digest: Sha256Digest,
}

/// Secret-free Agent configuration error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AgentConfigError;

impl fmt::Display for AgentConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Agent session configuration is invalid")
    }
}

impl std::error::Error for AgentConfigError {}

/// Resolves the exact Agent identity and effective profile inherited by a Session.
///
/// The stable identity excludes process-specific and Session-specific ids. The
/// profile revision includes every effective runtime setting, so a change is
/// visible and never mutates an already stored Session snapshot.
///
/// # Errors
///
/// Rejects empty, unbounded, or control-bearing values and canonicalizes tools.
pub fn resolve_agent_session_config(
    worker_id: &WorkerId,
    capabilities: &WorkerCapabilitySet,
    execution_profile: &str,
    mut settings: AgentProfileSettings,
) -> Result<AgentSessionConfigSnapshot, AgentConfigError> {
    validate_text(worker_id.0.as_str(), 200)?;
    validate_text(execution_profile, 100)?;
    validate_text(&settings.provider, 200)?;
    validate_text(&settings.model, 200)?;
    validate_text(&settings.reasoning, 100)?;
    validate_text(&settings.sandbox, 100)?;
    if let Some(instructions) = settings.instructions.as_deref() {
        validate_text(instructions, 65_536)?;
    }
    if settings.tools.len() > 256 {
        return Err(AgentConfigError);
    }
    for tool in &settings.tools {
        validate_text(tool, 200)?;
    }
    settings.tools.sort();
    settings.tools.dedup();

    let identity_digest = digest(&(worker_id, execution_profile))?;
    let identity = AgentIdentity {
        id: format!("agt_{}", &identity_digest.0[7..33].to_ascii_uppercase()),
        worker_id: worker_id.clone(),
        name: profile_name(execution_profile),
        role: execution_profile.to_owned(),
        capabilities: capabilities.clone(),
    };
    let source = AgentProfileSource {
        execution_profile: execution_profile.to_owned(),
        worker_capability_digest: capabilities.capability_digest.clone(),
        settings,
    };
    let profile = AgentProfile {
        revision: digest(&source)?,
        source,
    };
    let snapshot_digest = digest(&(AGENT_SESSION_CONFIG_SCHEMA_VERSION, &identity, &profile))?;
    Ok(AgentSessionConfigSnapshot {
        schema_version: AGENT_SESSION_CONFIG_SCHEMA_VERSION,
        identity,
        profile,
        snapshot_digest,
    })
}

/// Verifies that a stored snapshot is exactly reproducible from its own source.
///
/// # Errors
///
/// Rejects altered identity, profile, settings, capability, or digest fields.
pub fn validate_agent_session_config(
    snapshot: &AgentSessionConfigSnapshot,
) -> Result<(), AgentConfigError> {
    let rebuilt = resolve_agent_session_config(
        &snapshot.identity.worker_id,
        &snapshot.identity.capabilities,
        &snapshot.identity.role,
        snapshot.profile.source.settings.clone(),
    )?;
    if rebuilt != *snapshot {
        return Err(AgentConfigError);
    }
    Ok(())
}

fn validate_text(value: &str, max_chars: usize) -> Result<(), AgentConfigError> {
    if value.trim().is_empty()
        || value.chars().count() > max_chars
        || value.chars().any(char::is_control)
    {
        return Err(AgentConfigError);
    }
    Ok(())
}

fn digest(value: &impl Serialize) -> Result<Sha256Digest, AgentConfigError> {
    let bytes = serde_json::to_vec(value).map_err(|_| AgentConfigError)?;
    Ok(Sha256Digest(format!("sha256:{:x}", Sha256::digest(bytes))))
}

fn profile_name(profile: &str) -> String {
    let mut characters = profile.chars();
    characters.next().map_or_else(
        || profile.to_owned(),
        |first| first.to_uppercase().chain(characters).collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::generated::{WorkerCapabilityFeature, WorkerCapabilitySetPlatform};

    fn capabilities(digest: char) -> WorkerCapabilitySet {
        WorkerCapabilitySet {
            capability_digest: Sha256Digest(format!("sha256:{}", digest.to_string().repeat(64))),
            features: vec![
                WorkerCapabilityFeature::Shell,
                WorkerCapabilityFeature::Sandbox,
            ],
            max_concurrent_jobs: 2,
            platform: WorkerCapabilitySetPlatform::Aarch64AppleDarwin,
        }
    }

    fn settings(model: &str) -> AgentProfileSettings {
        AgentProfileSettings {
            provider: "fixture-provider".to_owned(),
            model: model.to_owned(),
            reasoning: "provider_default".to_owned(),
            tools: vec!["shell".to_owned(), "mcp://fixture/read".to_owned()],
            sandbox: "candidate-write".to_owned(),
            instructions: Some("Implement the current task.".to_owned()),
        }
    }

    #[test]
    fn identity_is_restart_stable_and_profile_changes_are_explainable() {
        let worker = WorkerId("wrk_agent_fixture".into());
        let first = resolve_agent_session_config(
            &worker,
            &capabilities('a'),
            "executor",
            settings("fixture-model-a"),
        )
        .unwrap();
        let restart = resolve_agent_session_config(
            &worker,
            &capabilities('a'),
            "executor",
            settings("fixture-model-a"),
        )
        .unwrap();
        assert_eq!(first, restart);
        assert_eq!(first.identity.name, "Executor");

        let changed = resolve_agent_session_config(
            &worker,
            &capabilities('a'),
            "executor",
            settings("fixture-model-b"),
        )
        .unwrap();
        assert_eq!(first.identity.id, changed.identity.id);
        assert_ne!(first.profile.revision, changed.profile.revision);
        assert_eq!(changed.profile.source.settings.model, "fixture-model-b");
    }
}
