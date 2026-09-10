// SPDX-License-Identifier: Apache-2.0

//! Model and Provider Policy routing and enforcement over durable retry facts.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use winwincode_api::generated::ModelRouteAvailabilityStatus;
use winwincode_domain::Sha256Digest;
use winwincode_execution_port::agent_config::AgentProfile;

use crate::{
    FrozenModelRetryPlan, FrozenModelRouteAuthority, ModelRetryStep, ModelRouteResolutionReason,
    ModelRouteResolutionTrace,
};

const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
const MAX_POLICY_ROUTES: usize = 16;

/// Provider/model identity used by one routing policy.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProviderModelSelector {
    pub provider_id: String,
    pub model_id: String,
}

/// One primary or fallback route with its hard attempt bound.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProviderPolicyRoute {
    pub selector: ProviderModelSelector,
    pub max_attempts: u64,
}

/// Route and reasoning constraints for one Agent execution profile.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentProviderPolicy {
    pub primary: ProviderPolicyRoute,
    pub fallbacks: Vec<ProviderPolicyRoute>,
    pub allowed_models: BTreeSet<ProviderModelSelector>,
    pub allowed_reasoning_efforts: BTreeSet<String>,
    /// Unknown Provider capacity is used only when this is explicitly true.
    pub allow_unknown_availability: bool,
}

/// Revisioned primary/fallback policy keyed by stable Agent profile name.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderPolicy {
    policy_id: String,
    revision: u64,
    profiles: BTreeMap<String, AgentProviderPolicy>,
    fingerprint: Sha256Digest,
}

/// One verified route candidate and its current runtime state.
#[derive(Clone, Debug, PartialEq)]
pub struct ProviderPolicyCandidate {
    pub authority: FrozenModelRouteAuthority,
    pub availability: ModelRouteAvailabilityStatus,
}

impl ProviderPolicy {
    /// Builds one immutable policy and rejects ambiguous or unusable profiles.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed, duplicate, or unusable policy data.
    pub fn try_new(
        policy_id: String,
        revision: u64,
        profiles: BTreeMap<String, AgentProviderPolicy>,
    ) -> Result<Self, ProviderPolicyError> {
        validate_token(&policy_id, 200)?;
        if revision == 0 || revision > MAX_SAFE_INTEGER || profiles.is_empty() {
            return Err(ProviderPolicyError::new(
                ProviderPolicyErrorKind::Unavailable,
            ));
        }
        for (profile, policy) in &profiles {
            validate_token(profile, 100)?;
            validate_agent_policy(policy)?;
        }
        let bytes = serde_json::to_vec(&(&policy_id, revision, &profiles))
            .map_err(|_| ProviderPolicyError::new(ProviderPolicyErrorKind::Unavailable))?;
        Ok(Self {
            policy_id,
            revision,
            profiles,
            fingerprint: Sha256Digest(format!("sha256:{:x}", Sha256::digest(bytes))),
        })
    }

    /// Re-resolves the first route from current availability while preserving
    /// the same policy and Agent profile revision.
    ///
    /// # Errors
    ///
    /// Returns an error when policy rejects the profile or no route is usable.
    pub fn resolve(
        &self,
        profile: &AgentProfile,
        candidates: &[ProviderPolicyCandidate],
        previous: Option<&ModelRouteResolutionTrace>,
    ) -> Result<FrozenModelRetryPlan, ProviderPolicyError> {
        let policy = self
            .profiles
            .get(&profile.source.execution_profile)
            .ok_or_else(|| ProviderPolicyError::new(ProviderPolicyErrorKind::Rejected))?;
        let selected_model = ProviderModelSelector {
            provider_id: profile.source.settings.provider.clone(),
            model_id: profile.source.settings.model.clone(),
        };
        if !policy.allowed_models.contains(&selected_model)
            || !policy
                .allowed_reasoning_efforts
                .contains(&profile.source.settings.reasoning)
        {
            return Err(ProviderPolicyError::new(ProviderPolicyErrorKind::Rejected));
        }

        let mut candidates_by_selector = BTreeMap::new();
        for candidate in candidates {
            let selector = ProviderModelSelector {
                provider_id: candidate.authority.route().provider_id.clone(),
                model_id: candidate.authority.route().model_id.clone(),
            };
            if candidates_by_selector.insert(selector, candidate).is_some() {
                return Err(ProviderPolicyError::new(
                    ProviderPolicyErrorKind::Unavailable,
                ));
            }
        }

        let configured = std::iter::once(&policy.primary)
            .chain(policy.fallbacks.iter())
            .collect::<Vec<_>>();
        let mut steps = Vec::new();
        let mut source_facts = Vec::new();
        let mut selected_index = None;
        for (index, route) in configured.into_iter().enumerate() {
            let Some(candidate) = candidates_by_selector.get(&route.selector) else {
                continue;
            };
            source_facts.push((
                &route.selector,
                candidate.authority.fingerprint(),
                candidate.availability.clone(),
            ));
            if candidate.availability != ModelRouteAvailabilityStatus::Available
                && !(policy.allow_unknown_availability
                    && candidate.availability == ModelRouteAvailabilityStatus::Unknown)
            {
                continue;
            }
            selected_index.get_or_insert(index);
            steps.push(
                ModelRetryStep::try_new(candidate.authority.clone(), route.max_attempts)
                    .map_err(|_| ProviderPolicyError::new(ProviderPolicyErrorKind::Unavailable))?,
            );
        }
        let selected_index = selected_index
            .ok_or_else(|| ProviderPolicyError::new(ProviderPolicyErrorKind::Rejected))?;
        let selected = steps[0].authority().fingerprint().to_owned();
        let previous_route_fingerprint =
            previous.map(|trace| trace.selected_route_fingerprint.clone());
        let reason = previous.map_or_else(
            || {
                if selected_index == 0 {
                    ModelRouteResolutionReason::PrimarySelected
                } else {
                    ModelRouteResolutionReason::FallbackSelected
                }
            },
            |trace| {
                if trace.selected_route_fingerprint == selected {
                    ModelRouteResolutionReason::ReplacementRetained
                } else {
                    ModelRouteResolutionReason::ReplacementReselected
                }
            },
        );
        let source = serde_json::to_vec(&(
            &self.fingerprint,
            &profile.revision,
            source_facts,
            &previous_route_fingerprint,
        ))
        .map_err(|_| ProviderPolicyError::new(ProviderPolicyErrorKind::Unavailable))?;
        FrozenModelRetryPlan::freeze_resolved(
            self.policy_id.clone(),
            self.revision,
            ModelRouteResolutionTrace {
                source_fingerprint: Sha256Digest(format!("sha256:{:x}", Sha256::digest(source))),
                selected_route_fingerprint: selected,
                previous_route_fingerprint,
                reason,
            },
            steps,
        )
        .map_err(|_| ProviderPolicyError::new(ProviderPolicyErrorKind::Unavailable))
    }
}

fn validate_agent_policy(policy: &AgentProviderPolicy) -> Result<(), ProviderPolicyError> {
    let routes = std::iter::once(&policy.primary)
        .chain(policy.fallbacks.iter())
        .collect::<Vec<_>>();
    if routes.len() > MAX_POLICY_ROUTES || policy.allowed_models.is_empty() {
        return Err(ProviderPolicyError::new(
            ProviderPolicyErrorKind::Unavailable,
        ));
    }
    let mut unique = BTreeSet::new();
    let mut total_attempts = 0_u64;
    for route in routes {
        validate_token(&route.selector.provider_id, 128)?;
        validate_token(&route.selector.model_id, 200)?;
        total_attempts = total_attempts
            .checked_add(route.max_attempts)
            .ok_or_else(|| ProviderPolicyError::new(ProviderPolicyErrorKind::Unavailable))?;
        if route.max_attempts == 0
            || total_attempts > MAX_POLICY_ROUTES as u64
            || !policy.allowed_models.contains(&route.selector)
            || !unique.insert(&route.selector)
        {
            return Err(ProviderPolicyError::new(
                ProviderPolicyErrorKind::Unavailable,
            ));
        }
    }
    if policy.allowed_reasoning_efforts.is_empty()
        || policy
            .allowed_reasoning_efforts
            .iter()
            .any(|effort| validate_token(effort, 100).is_err())
    {
        return Err(ProviderPolicyError::new(
            ProviderPolicyErrorKind::Unavailable,
        ));
    }
    Ok(())
}

fn validate_token(value: &str, max_chars: usize) -> Result<(), ProviderPolicyError> {
    if value.trim().is_empty()
        || value.chars().count() > max_chars
        || value.chars().any(char::is_control)
    {
        Err(ProviderPolicyError::new(
            ProviderPolicyErrorKind::Unavailable,
        ))
    } else {
        Ok(())
    }
}

/// Stable Provider Policy enforcement failure category.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderPolicyErrorKind {
    Rejected,
    Unavailable,
}

/// Bounded Provider Policy error which retains no model input or Credential.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderPolicyError {
    kind: ProviderPolicyErrorKind,
}

impl ProviderPolicyError {
    const fn new(kind: ProviderPolicyErrorKind) -> Self {
        Self { kind }
    }

    /// Returns the stable failure category.
    #[must_use]
    pub const fn kind(&self) -> ProviderPolicyErrorKind {
        self.kind
    }
}

impl fmt::Display for ProviderPolicyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Provider Policy evaluation failed")
    }
}

impl std::error::Error for ProviderPolicyError {}
