use std::fmt;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use winwincode_domain::{Sha256Digest, WorkerId};

use crate::generated::WorkerCapabilitySet;

pub const AGENT_SESSION_CONFIG_SCHEMA_VERSION: u64 = 1;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct AgentIdentity {
    pub id: String,
    pub worker_id: WorkerId,
    pub name: String,
    pub role: String,
    pub capabilities: WorkerCapabilitySet,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct AgentJevContextSettings {
    pub provider: String,
    pub policy: crate::jev_decision::JevPolicy,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct AgentFusionMember {
    pub id: String,
    pub provider: String,
    pub model: String,
    pub reasoning: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct AgentFusionSettings {
    pub members: Vec<AgentFusionMember>,
}

pub fn validate_fusion_settings(settings: &AgentFusionSettings) -> Result<(), AgentConfigError> {
    if !(3..=16).contains(&settings.members.len()) {
        return Err(AgentConfigError);
    }
    let mut ids = std::collections::BTreeSet::new();
    let mut routes = std::collections::BTreeSet::new();
    let mut providers = std::collections::BTreeSet::new();
    for member in &settings.members {
        for field in [&member.id, &member.provider, &member.model] {
            validate_text(field, 200)?;
        }
        validate_text(&member.reasoning, 100)?;
        if !ids.insert(&member.id) || !routes.insert((&member.provider, &member.model)) {
            return Err(AgentConfigError);
        }
        providers.insert(&member.provider);
    }
    if providers.len() < 3 {
        return Err(AgentConfigError);
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct AgentProfileSettings {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fusion: Option<AgentFusionSettings>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub jev_context: Option<AgentJevContextSettings>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub jev_judge: Option<String>,
    pub provider: String,
    pub model: String,
    pub reasoning: String,
    pub tools: Vec<String>,
    pub sandbox: String,
    pub instructions: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct AgentProfileSource {
    pub execution_profile: String,
    pub worker_capability_digest: Sha256Digest,
    pub settings: AgentProfileSettings,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct AgentProfile {
    pub revision: Sha256Digest,
    pub source: AgentProfileSource,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct AgentSessionConfigSnapshot {
    pub schema_version: u64,
    pub identity: AgentIdentity,
    pub profile: AgentProfile,
    pub snapshot_digest: Sha256Digest,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AgentConfigError;

impl fmt::Display for AgentConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Agent session configuration is invalid")
    }
}

impl std::error::Error for AgentConfigError {}

pub fn resolve_agent_session_config(
    worker_id: &WorkerId,
    capabilities: &WorkerCapabilitySet,
    execution_profile: &str,
    mut settings: AgentProfileSettings,
) -> Result<AgentSessionConfigSnapshot, AgentConfigError> {
    validate_text(worker_id.0.as_str(), 200)?;
    validate_text(execution_profile, 100)?;
    validate_text(&settings.provider, 200)?;
    if let Some(fusion) = &settings.fusion {
        validate_fusion_settings(fusion)?;
    }
    if let Some(provider) = &settings.jev_judge {
        validate_text(provider, 200)?;
    }
    if let Some(context) = &settings.jev_context {
        validate_text(&context.provider, 200)?;
        crate::jev_decision::validate_policy(&context.policy).map_err(|_| AgentConfigError)?;
    }
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
            fusion: None,
            jev_judge: None,
            jev_context: None,
            provider: "fixture-provider".to_owned(),
            model: model.to_owned(),
            reasoning: "provider_default".to_owned(),
            tools: vec!["shell".to_owned(), "mcp://fixture/read".to_owned()],
            sandbox: "candidate-write".to_owned(),
            instructions: Some("Implement the current task.".to_owned()),
        }
    }

    #[test]
    fn judge_route_is_independently_sealed_and_tampering_is_rejected() {
        let resolve = |settings| {
            resolve_agent_session_config(
                &WorkerId("wrk_agent_fixture".into()),
                &capabilities('a'),
                "executor",
                settings,
            )
        };
        let baseline = resolve(settings("fixture-model")).unwrap();
        let mut configured = settings("fixture-model");
        configured.jev_judge = Some("device-judge".into());
        let mut sealed = resolve(configured.clone()).unwrap();
        assert_ne!(baseline.snapshot_digest, sealed.snapshot_digest);
        assert!(sealed.profile.source.settings.jev_context.is_none());
        sealed.profile.source.settings.jev_judge = Some("changed".into());
        assert!(validate_agent_session_config(&sealed).is_err());
        configured.jev_judge = Some("\n".into());
        assert!(resolve(configured).is_err());
    }

    #[test]
    fn jev_context_policy_is_sealed_and_invalid_policy_is_rejected() {
        let resolve = |settings| {
            resolve_agent_session_config(
                &WorkerId("wrk_agent_fixture".into()),
                &capabilities('a'),
                "executor",
                settings,
            )
        };
        let mut configured = settings("fixture-model-a");
        let plain = resolve(configured.clone()).unwrap();
        configured.jev_context = Some(AgentJevContextSettings {
            provider: "device-jev".into(),
            policy: crate::jev_decision::JevPolicy {
                version: "jev-policy.v1".into(),
                minimum_confidence: 0.55,
                pin_threshold: 0.90,
                keep_threshold: 0.75,
                compact_threshold: 0.75,
                drop_threshold: 0.90,
                task_memory_threshold: 0.40,
                project_memory_threshold: 0.60,
                long_term_memory_threshold: 0.80,
            },
        });
        let sealed = resolve(configured.clone()).unwrap();
        assert_ne!(plain.profile.revision, sealed.profile.revision);
        let stored: AgentSessionConfigSnapshot =
            serde_json::from_slice(&serde_json::to_vec(&sealed).unwrap()).unwrap();
        validate_agent_session_config(&stored).unwrap();
        let mut tampered = stored;
        tampered
            .profile
            .source
            .settings
            .jev_context
            .as_mut()
            .unwrap()
            .policy
            .drop_threshold = 0.95;
        assert!(validate_agent_session_config(&tampered).is_err());
        configured
            .jev_context
            .as_mut()
            .unwrap()
            .policy
            .drop_threshold = 1.1;
        assert!(resolve(configured).is_err());
        assert!(
            serde_json::to_value(plain.profile.source.settings)
                .unwrap()
                .get("jevContext")
                .is_none()
        );
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
