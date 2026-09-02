// SPDX-License-Identifier: Apache-2.0

//! Enterprise constraints over the canonical Worker placement policy.
//!
//! This module compiles authenticated pool and repository metadata into the
//! generic deterministic placement engine. It does not own Worker identity,
//! capacity, leases, fencing, or pool attribution. A selected placement can
//! only be claimed through [`crate::ExecutionRegistry`], which revalidates the
//! current process instance and freezes the authenticated pool beside the
//! accepted lease.

use std::collections::{HashMap, HashSet};

use winwincode_domain::{
    ExecutionJobId, ExecutionMessageId, FencingToken, Instant, LeaseId, RequestId, Sha256Digest,
};

use crate::worker_placement::{
    WorkerPlacementConstraintEvaluator, place_worker_batch_with_constraints,
};
use crate::{
    AuthenticatedWorkerPlacement, ExecutionLeaseClaim, ExecutionLeaseReceipt, SqliteStorage,
    StorageError, WorkerCapacityEntry, WorkerPlacementCandidate, WorkerPlacementDecision,
    WorkerPlacementError, WorkerPlacementFailure, WorkerPlacementQuota, WorkerPlacementRejection,
    WorkerPlacementRequest, WorkerPlacementSelection, WorkerPoolId, WorkerRegistryScope,
    WorkerRepositoryAccess,
};

const MAX_CONSTRAINT_VALUES: usize = 64;
const MAX_CONSTRAINT_VALUE_BYTES: usize = 128;

/// Ordered enterprise isolation tier for one authenticated Worker pool.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum EnterpriseWorkerSecurityTier {
    Standard,
    Restricted,
    Confidential,
}

impl EnterpriseWorkerSecurityTier {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Standard => "standard",
            Self::Restricted => "restricted",
            Self::Confidential => "confidential",
        }
    }
}

/// Trusted enterprise metadata attached to one authenticated Worker pool.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EnterpriseWorkerPoolProfile {
    pub scope: WorkerRegistryScope,
    pub worker_pool_id: WorkerPoolId,
    pub region: String,
    pub security_tier: EnterpriseWorkerSecurityTier,
    pub network_zones: Vec<String>,
    pub plugins: Vec<String>,
    pub repository_capabilities: Vec<String>,
    pub repository_access: Vec<WorkerRepositoryAccess>,
}

/// One candidate built from Registry capacity, authenticated placement, and
/// its exact enterprise pool profile.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EnterpriseWorkerPlacementCandidate {
    placement: WorkerPlacementCandidate,
    authenticated_placement: AuthenticatedWorkerPlacement,
    profile: EnterpriseWorkerPoolProfile,
}

impl EnterpriseWorkerPlacementCandidate {
    /// Joins one current Worker process to its authenticated pool profile.
    ///
    /// # Errors
    ///
    /// Rejects changed process identities, foreign scopes or pools, malformed
    /// metadata, and repository access outside the authenticated boundary.
    pub fn from_authority(
        worker: WorkerCapacityEntry,
        authenticated_placement: AuthenticatedWorkerPlacement,
        profile: EnterpriseWorkerPoolProfile,
    ) -> Result<Self, WorkerPlacementError> {
        validate_profile(&profile)?;
        if worker.worker_id != authenticated_placement.worker_id
            || worker.worker_instance_id != authenticated_placement.worker_instance_id
            || profile.worker_pool_id != authenticated_placement.worker_pool_id
            || profile.scope != authenticated_placement.management_scope
        {
            return Err(WorkerPlacementError::invalid(
                "enterprise Worker profile differs from authenticated placement authority",
            ));
        }
        Ok(Self {
            placement: WorkerPlacementCandidate {
                worker,
                network_zones: profile.network_zones.clone(),
                repository_access: profile.repository_access.clone(),
            },
            authenticated_placement,
            profile,
        })
    }

    #[must_use]
    pub const fn worker(&self) -> &WorkerCapacityEntry {
        &self.placement.worker
    }

    #[must_use]
    pub const fn authenticated_placement(&self) -> &AuthenticatedWorkerPlacement {
        &self.authenticated_placement
    }

    #[must_use]
    pub const fn profile(&self) -> &EnterpriseWorkerPoolProfile {
        &self.profile
    }
}

/// One execution request plus its enterprise pool constraints.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EnterpriseWorkerPlacementRequest {
    pub placement: WorkerPlacementRequest,
    pub allowed_worker_pools: Vec<WorkerPoolId>,
    pub allowed_regions: Vec<String>,
    pub minimum_security_tier: EnterpriseWorkerSecurityTier,
    pub required_plugins: Vec<String>,
    pub required_repository_capabilities: Vec<String>,
}

/// Selected placement minted only by the enterprise constraint compiler.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EnterpriseWorkerPlacementSelection {
    placement: WorkerPlacementSelection,
}

impl EnterpriseWorkerPlacementSelection {
    #[must_use]
    pub const fn placement(&self) -> &WorkerPlacementSelection {
        &self.placement
    }
}

/// Explicit scheduler outcome. A rejected decision remains queued with every
/// stable per-Worker and global explanation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EnterpriseWorkerPlacementDecision {
    Placed(EnterpriseWorkerPlacementSelection),
    Queued(WorkerPlacementRejection),
}

/// Lease fields that cannot be selected or overridden by a Worker candidate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EnterpriseWorkerLeaseClaim {
    pub expires_at: Instant,
    pub fencing_token: FencingToken,
    pub issued_at: Instant,
    pub job_id: ExecutionJobId,
    pub lease_id: LeaseId,
    pub message_id: ExecutionMessageId,
    pub payload_digest: Sha256Digest,
    pub request_id: RequestId,
    pub attempt: u64,
}

/// Places a batch through the one generic deterministic ranking policy.
///
/// Requests are sorted by execution Job identity. Enterprise constraints are
/// evaluated as hard conditions before the generic policy can select a
/// Worker. Capacity and exact-scope quota are decremented only in the generic
/// batch snapshot, so caller order cannot change the result.
///
/// # Errors
///
/// Rejects malformed or ambiguous profiles, constraints, and generic
/// placement snapshots.
pub fn place_enterprise_worker_batch(
    requests: &[EnterpriseWorkerPlacementRequest],
    candidates: &[EnterpriseWorkerPlacementCandidate],
    quotas: &[WorkerPlacementQuota],
) -> Result<Vec<EnterpriseWorkerPlacementDecision>, WorkerPlacementError> {
    validate_enterprise_inputs(requests, candidates)?;
    let constraints = EnterpriseConstraints::new(requests, candidates);
    let placement_requests = requests
        .iter()
        .map(|request| request.placement.clone())
        .collect::<Vec<_>>();
    let placement_candidates = candidates
        .iter()
        .map(|candidate| candidate.placement.clone())
        .collect::<Vec<_>>();
    place_worker_batch_with_constraints(
        &placement_requests,
        &placement_candidates,
        quotas,
        &constraints,
    )
    .map(|decisions| {
        decisions
            .into_iter()
            .map(|decision| match decision {
                WorkerPlacementDecision::Selected(placement) => {
                    EnterpriseWorkerPlacementDecision::Placed(EnterpriseWorkerPlacementSelection {
                        placement,
                    })
                }
                WorkerPlacementDecision::Rejected(rejection) => {
                    EnterpriseWorkerPlacementDecision::Queued(rejection)
                }
            })
            .collect()
    })
}

/// Claims one selected placement through the canonical Registry lease/fence
/// transaction.
///
/// # Errors
///
/// Rejects a mismatched Job identity and propagates the Registry's durable
/// claim result or storage failure. The Registry revalidates current Worker
/// instance and authenticated pool authority before writing the lease.
pub fn claim_enterprise_worker_selection(
    storage: &mut SqliteStorage,
    selection: &EnterpriseWorkerPlacementSelection,
    claim: &EnterpriseWorkerLeaseClaim,
) -> Result<ExecutionLeaseReceipt, StorageError> {
    if selection.placement.job_id != claim.job_id {
        return Err(StorageError::invalid_input(
            "enterprise placement selection belongs to another execution Job",
        ));
    }
    storage
        .execution_registry()?
        .claim_execution_job_with_authenticated_placement(&ExecutionLeaseClaim {
            expires_at: claim.expires_at.clone(),
            fencing_token: claim.fencing_token.clone(),
            issued_at: claim.issued_at.clone(),
            job_id: claim.job_id.clone(),
            lease_id: claim.lease_id.clone(),
            message_id: claim.message_id.clone(),
            payload_digest: claim.payload_digest.clone(),
            request_id: claim.request_id.clone(),
            worker_id: selection.placement.worker_id.clone(),
            worker_instance_id: selection.placement.worker_instance_id.clone(),
            attempt: claim.attempt,
        })
}

struct EnterpriseConstraints<'input> {
    requests: HashMap<&'input str, &'input EnterpriseWorkerPlacementRequest>,
    candidates: HashMap<(&'input str, &'input str), &'input EnterpriseWorkerPlacementCandidate>,
}

impl<'input> EnterpriseConstraints<'input> {
    fn new(
        requests: &'input [EnterpriseWorkerPlacementRequest],
        candidates: &'input [EnterpriseWorkerPlacementCandidate],
    ) -> Self {
        Self {
            requests: requests
                .iter()
                .map(|request| (request.placement.job_id.0.as_str(), request))
                .collect(),
            candidates: candidates
                .iter()
                .map(|candidate| {
                    (
                        (
                            candidate.placement.worker.worker_id.0.as_str(),
                            candidate.placement.worker.worker_instance_id.0.as_str(),
                        ),
                        candidate,
                    )
                })
                .collect(),
        }
    }
}

impl WorkerPlacementConstraintEvaluator for EnterpriseConstraints<'_> {
    fn additional_failures(
        &self,
        request: &WorkerPlacementRequest,
        candidate: &WorkerPlacementCandidate,
    ) -> Vec<WorkerPlacementFailure> {
        let Some(requirements) = self.requests.get(request.job_id.0.as_str()) else {
            return vec![WorkerPlacementFailure::EnterpriseConstraintAuthorityUnavailable];
        };
        let Some(candidate) = self.candidates.get(&(
            candidate.worker.worker_id.0.as_str(),
            candidate.worker.worker_instance_id.0.as_str(),
        )) else {
            return vec![WorkerPlacementFailure::EnterpriseConstraintAuthorityUnavailable];
        };
        enterprise_failures(requirements, candidate)
    }
}

fn enterprise_failures(
    request: &EnterpriseWorkerPlacementRequest,
    candidate: &EnterpriseWorkerPlacementCandidate,
) -> Vec<WorkerPlacementFailure> {
    let mut failures = Vec::new();
    if !request
        .allowed_worker_pools
        .contains(&candidate.profile.worker_pool_id)
    {
        failures.push(WorkerPlacementFailure::WorkerPoolNotAllowed(
            candidate.profile.worker_pool_id.clone(),
        ));
    }
    if !request.allowed_regions.contains(&candidate.profile.region) {
        failures.push(WorkerPlacementFailure::RegionNotAllowed(
            candidate.profile.region.clone(),
        ));
    }
    if candidate.profile.security_tier < request.minimum_security_tier {
        failures.push(WorkerPlacementFailure::SecurityTierInsufficient {
            required: request.minimum_security_tier.as_str().to_owned(),
            actual: candidate.profile.security_tier.as_str().to_owned(),
        });
    }
    let mut missing_plugins = request
        .required_plugins
        .iter()
        .filter(|plugin| !candidate.profile.plugins.contains(plugin))
        .cloned()
        .collect::<Vec<_>>();
    missing_plugins.sort_unstable();
    failures.extend(
        missing_plugins
            .into_iter()
            .map(WorkerPlacementFailure::MissingPlugin),
    );
    let mut missing_repository_capabilities = request
        .required_repository_capabilities
        .iter()
        .filter(|capability| {
            !candidate
                .profile
                .repository_capabilities
                .contains(capability)
        })
        .cloned()
        .collect::<Vec<_>>();
    missing_repository_capabilities.sort_unstable();
    failures.extend(
        missing_repository_capabilities
            .into_iter()
            .map(WorkerPlacementFailure::MissingRepositoryCapability),
    );
    failures
}

fn validate_enterprise_inputs(
    requests: &[EnterpriseWorkerPlacementRequest],
    candidates: &[EnterpriseWorkerPlacementCandidate],
) -> Result<(), WorkerPlacementError> {
    let mut request_ids = HashSet::new();
    for request in requests {
        if !request_ids.insert(request.placement.job_id.0.as_str())
            || request.allowed_worker_pools.is_empty()
            || request.allowed_regions.is_empty()
        {
            return Err(WorkerPlacementError::invalid(
                "enterprise placement requirements are ambiguous",
            ));
        }
        validate_unique_pool_ids(&request.allowed_worker_pools)?;
        validate_constraint_values(&request.allowed_regions)?;
        validate_constraint_values(&request.required_plugins)?;
        validate_constraint_values(&request.required_repository_capabilities)?;
    }
    let mut worker_instances = HashSet::new();
    for candidate in candidates {
        if !worker_instances.insert((
            candidate.placement.worker.worker_id.0.as_str(),
            candidate.placement.worker.worker_instance_id.0.as_str(),
        )) {
            return Err(WorkerPlacementError::invalid(
                "enterprise Fleet contains a duplicate Worker process",
            ));
        }
        validate_profile(&candidate.profile)?;
    }
    Ok(())
}

fn validate_profile(profile: &EnterpriseWorkerPoolProfile) -> Result<(), WorkerPlacementError> {
    validate_pool_id(&profile.worker_pool_id)?;
    validate_constraint_value(&profile.region)?;
    validate_constraint_values(&profile.network_zones)?;
    validate_constraint_values(&profile.plugins)?;
    validate_constraint_values(&profile.repository_capabilities)?;
    if profile.network_zones.is_empty() || profile.repository_access.is_empty() {
        return Err(WorkerPlacementError::invalid(
            "enterprise Worker pool profile has no reachable boundary",
        ));
    }
    if profile
        .repository_access
        .iter()
        .any(|access| !repository_access_within_scope(access, &profile.scope))
    {
        return Err(WorkerPlacementError::invalid(
            "enterprise Worker repository access crosses its authenticated scope",
        ));
    }
    Ok(())
}

fn repository_access_within_scope(
    access: &WorkerRepositoryAccess,
    scope: &WorkerRegistryScope,
) -> bool {
    match scope {
        WorkerRegistryScope::Organization { organization_id } => {
            access.organization_id == *organization_id
        }
        WorkerRegistryScope::Workspace {
            organization_id,
            workspace_id,
        } => access.organization_id == *organization_id && access.workspace_id == *workspace_id,
        WorkerRegistryScope::Project {
            organization_id,
            workspace_id,
            project_id,
        } => {
            access.organization_id == *organization_id
                && access.workspace_id == *workspace_id
                && access.project_id == *project_id
        }
        WorkerRegistryScope::Repository {
            organization_id,
            workspace_id,
            project_id,
            repository_id,
        } => {
            access.organization_id == *organization_id
                && access.workspace_id == *workspace_id
                && access.project_id == *project_id
                && access.repository_id == *repository_id
        }
    }
}

fn validate_unique_pool_ids(pool_ids: &[WorkerPoolId]) -> Result<(), WorkerPlacementError> {
    if pool_ids.len() > MAX_CONSTRAINT_VALUES {
        return Err(WorkerPlacementError::invalid(
            "enterprise placement pool constraint exceeds its bound",
        ));
    }
    let mut unique = HashSet::new();
    for pool_id in pool_ids {
        validate_pool_id(pool_id)?;
        if !unique.insert(pool_id.0.as_str()) {
            return Err(WorkerPlacementError::invalid(
                "enterprise placement pool constraint contains duplicates",
            ));
        }
    }
    Ok(())
}

fn validate_pool_id(pool_id: &WorkerPoolId) -> Result<(), WorkerPlacementError> {
    let valid = pool_id.0.strip_prefix("wpl_").is_some_and(|suffix| {
        suffix.len() == 26
            && suffix.bytes().all(|byte| {
                byte.is_ascii_digit()
                    || matches!(
                        byte,
                        b'A'..=b'H' | b'J'..=b'K' | b'M'..=b'N' | b'P'..=b'T' | b'V'..=b'Z'
                    )
            })
    });
    if valid {
        Ok(())
    } else {
        Err(WorkerPlacementError::invalid(
            "enterprise Worker pool identity is invalid",
        ))
    }
}

fn validate_constraint_values(values: &[String]) -> Result<(), WorkerPlacementError> {
    if values.len() > MAX_CONSTRAINT_VALUES {
        return Err(WorkerPlacementError::invalid(
            "enterprise placement constraint exceeds its bound",
        ));
    }
    let mut unique = HashSet::new();
    for value in values {
        validate_constraint_value(value)?;
        if !unique.insert(value.as_str()) {
            return Err(WorkerPlacementError::invalid(
                "enterprise placement constraint contains duplicates",
            ));
        }
    }
    Ok(())
}

fn validate_constraint_value(value: &str) -> Result<(), WorkerPlacementError> {
    if value.is_empty()
        || value.len() > MAX_CONSTRAINT_VALUE_BYTES
        || value.trim() != value
        || !value.is_ascii()
        || value.bytes().any(|byte| byte.is_ascii_control())
    {
        Err(WorkerPlacementError::invalid(
            "enterprise placement constraint value is invalid",
        ))
    } else {
        Ok(())
    }
}
