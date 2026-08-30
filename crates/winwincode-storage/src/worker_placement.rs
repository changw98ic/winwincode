// SPDX-License-Identifier: Apache-2.0

//! Deterministic `Worker` placement over one registry capacity snapshot.
//!
//! Placement is deliberately a pure decision. It consumes registry-owned
//! `Worker` facts plus caller-owned reachability and quota snapshots, then
//! returns an explainable choice. It cannot create a lease, reserve durable
//! capacity, or replace [`crate::ExecutionRegistry`] as the authority that
//! decides whether the resulting claim commits.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::fmt;

use winwincode_domain::{
    ExecutionJobId, OrganizationId, ProductSessionId, ProjectId, RepositoryId, WorkerId,
    WorkerInstanceId, WorkspaceId,
};

use crate::{WorkerCapacityEntry, WorkerHealth, WorkerPlatform, WorkerPoolId};

/// Exact tenant and workspace in which a `Worker` may reach one repository.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkerRepositoryAccess {
    pub organization_id: OrganizationId,
    pub workspace_id: WorkspaceId,
    pub project_id: ProjectId,
    pub repository_id: RepositoryId,
}

/// Registry capacity plus non-registry reachability facts for one current
/// `Worker` process instance.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkerPlacementCandidate {
    pub worker: WorkerCapacityEntry,
    pub network_zones: Vec<String>,
    pub repository_access: Vec<WorkerRepositoryAccess>,
}

/// Exact scope bound to a reusable `ProductSession` workspace.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkerSessionAffinity {
    pub organization_id: OrganizationId,
    pub workspace_id: WorkspaceId,
    pub project_id: ProjectId,
    pub repository_id: RepositoryId,
    pub product_session_id: ProductSessionId,
    pub worker_id: WorkerId,
    pub worker_instance_id: WorkerInstanceId,
}

/// One job's hard placement requirements.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkerPlacementRequest {
    pub job_id: ExecutionJobId,
    pub organization_id: OrganizationId,
    pub workspace_id: WorkspaceId,
    pub project_id: ProjectId,
    pub repository_id: RepositoryId,
    pub product_session_id: ProductSessionId,
    pub protocol_version: String,
    pub platform: WorkerPlatform,
    pub required_capabilities: Vec<String>,
    pub network_zone: String,
    pub security_zone: String,
    pub affinity: Option<WorkerSessionAffinity>,
}

/// Caller-owned running quota for one exact placement scope.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkerPlacementQuota {
    pub organization_id: OrganizationId,
    pub workspace_id: WorkspaceId,
    pub project_id: ProjectId,
    pub repository_id: RepositoryId,
    pub product_session_id: ProductSessionId,
    pub max_running_slots: u64,
    pub running_slots: u64,
}

/// One hard condition that excluded a `Worker` process instance.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorkerPlacementFailure {
    ProtocolVersionMismatch {
        required: String,
        actual: String,
    },
    PlatformMismatch {
        required: WorkerPlatform,
        actual: WorkerPlatform,
    },
    MissingCapability(String),
    WorkerNotHealthy(WorkerHealth),
    SecurityZoneMismatch {
        required: String,
        actual: String,
    },
    NetworkZoneUnavailable(String),
    RepositoryUnreachable(RepositoryId),
    WorkspaceTenantMismatch {
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
    },
    WorkerPoolNotAllowed(WorkerPoolId),
    RegionNotAllowed(String),
    SecurityTierInsufficient {
        required: String,
        actual: String,
    },
    MissingPlugin(String),
    MissingRepositoryCapability(String),
    EnterpriseConstraintAuthorityUnavailable,
    NoAvailableCapacity,
}

/// Batch-wide condition that prevents every otherwise valid `Worker` choice.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorkerPlacementGlobalFailure {
    ScopeQuotaExhausted {
        max_running_slots: u64,
        running_slots: u64,
    },
}

/// Explicit reason an existing `ProductSession` workspace was not reused.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorkerAffinityFailure {
    ScopeMismatch,
    WorkerUnavailable,
    WorkerInstanceReplaced {
        current_worker_instance_id: WorkerInstanceId,
    },
    WorkerIneligible {
        failures: Vec<WorkerPlacementFailure>,
    },
}

/// Rejection details for one current `Worker` process instance.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkerPlacementCandidateRejection {
    pub worker_id: WorkerId,
    pub worker_instance_id: WorkerInstanceId,
    pub failures: Vec<WorkerPlacementFailure>,
}

/// Explainable selected placement. The remaining-slot values are only the
/// batch calculation; a subsequent registry lease claim remains authoritative.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkerPlacementSelection {
    pub job_id: ExecutionJobId,
    pub worker_id: WorkerId,
    pub worker_instance_id: WorkerInstanceId,
    pub reused_affinity: bool,
    pub affinity_failure: Option<WorkerAffinityFailure>,
    pub worker_available_slots_after: u64,
    pub scope_available_slots_after: u64,
}

/// Explainable failure after every current `Worker` was evaluated.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkerPlacementRejection {
    pub job_id: ExecutionJobId,
    pub affinity_failure: Option<WorkerAffinityFailure>,
    pub global_failures: Vec<WorkerPlacementGlobalFailure>,
    pub workers: Vec<WorkerPlacementCandidateRejection>,
}

/// Closed placement outcome for one requested execution job.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorkerPlacementDecision {
    Selected(WorkerPlacementSelection),
    Rejected(WorkerPlacementRejection),
}

/// Structurally invalid or ambiguous placement snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkerPlacementError {
    message: String,
}

impl WorkerPlacementError {
    pub(crate) fn invalid(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

pub(crate) trait WorkerPlacementConstraintEvaluator {
    fn additional_failures(
        &self,
        request: &WorkerPlacementRequest,
        candidate: &WorkerPlacementCandidate,
    ) -> Vec<WorkerPlacementFailure>;
}

struct NoAdditionalConstraints;

impl WorkerPlacementConstraintEvaluator for NoAdditionalConstraints {
    fn additional_failures(
        &self,
        _request: &WorkerPlacementRequest,
        _candidate: &WorkerPlacementCandidate,
    ) -> Vec<WorkerPlacementFailure> {
        Vec::new()
    }
}

impl fmt::Display for WorkerPlacementError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for WorkerPlacementError {}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct PlacementScopeKey {
    organization: String,
    workspace: String,
    project: String,
    repository: String,
    product_session: String,
}

impl PlacementScopeKey {
    fn request(request: &WorkerPlacementRequest) -> Self {
        Self {
            organization: request.organization_id.0.clone(),
            workspace: request.workspace_id.0.clone(),
            project: request.project_id.0.clone(),
            repository: request.repository_id.0.clone(),
            product_session: request.product_session_id.0.clone(),
        }
    }

    fn quota(quota: &WorkerPlacementQuota) -> Self {
        Self {
            organization: quota.organization_id.0.clone(),
            workspace: quota.workspace_id.0.clone(),
            project: quota.project_id.0.clone(),
            repository: quota.repository_id.0.clone(),
            product_session: quota.product_session_id.0.clone(),
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct QuotaState {
    max_running_slots: u64,
    running_slots: u64,
}

/// Places a batch against one immutable registry snapshot.
///
/// Jobs are evaluated in ascending `ExecutionJobId` order, independent of
/// caller slice order. A valid affinity wins before capacity ranking; fallback
/// selects the most available eligible `Worker`, then the lowest stable
/// `WorkerId` and `WorkerInstanceId`. Batch-local capacity and quota are
/// decremented after every selection so one visible slot selects at most one
/// job. No durable state is changed.
///
/// # Errors
///
/// Rejects duplicate identities, impossible capacity/quota values, a missing
/// exact-scope quota, and empty hard requirement values.
pub fn place_worker_batch(
    requests: &[WorkerPlacementRequest],
    candidates: &[WorkerPlacementCandidate],
    quotas: &[WorkerPlacementQuota],
) -> Result<Vec<WorkerPlacementDecision>, WorkerPlacementError> {
    place_worker_batch_with_constraints(requests, candidates, quotas, &NoAdditionalConstraints)
}

pub(crate) fn place_worker_batch_with_constraints(
    requests: &[WorkerPlacementRequest],
    candidates: &[WorkerPlacementCandidate],
    quotas: &[WorkerPlacementQuota],
    constraints: &dyn WorkerPlacementConstraintEvaluator,
) -> Result<Vec<WorkerPlacementDecision>, WorkerPlacementError> {
    validate_inputs(requests, candidates, quotas)?;

    let mut remaining_capacity = candidates
        .iter()
        .map(|candidate| (worker_key(candidate), candidate.worker.available_slots))
        .collect::<HashMap<_, _>>();
    let mut quota_states = quotas
        .iter()
        .map(|quota| {
            (
                PlacementScopeKey::quota(quota),
                QuotaState {
                    max_running_slots: quota.max_running_slots,
                    running_slots: quota.running_slots,
                },
            )
        })
        .collect::<HashMap<_, _>>();
    let mut ordered_requests = requests.iter().collect::<Vec<_>>();
    ordered_requests.sort_unstable_by(|left, right| left.job_id.0.cmp(&right.job_id.0));

    ordered_requests
        .into_iter()
        .map(|request| {
            place_one(
                request,
                candidates,
                &mut remaining_capacity,
                &mut quota_states,
                constraints,
            )
        })
        .collect()
}

fn place_one(
    request: &WorkerPlacementRequest,
    candidates: &[WorkerPlacementCandidate],
    remaining_capacity: &mut HashMap<(String, String), u64>,
    quota_states: &mut HashMap<PlacementScopeKey, QuotaState>,
    constraints: &dyn WorkerPlacementConstraintEvaluator,
) -> Result<WorkerPlacementDecision, WorkerPlacementError> {
    let scope_key = PlacementScopeKey::request(request);
    let quota = quota_states.get(&scope_key).copied().ok_or_else(|| {
        WorkerPlacementError::invalid("a placement request has no exact-scope quota")
    })?;
    let quota_available = quota.max_running_slots.saturating_sub(quota.running_slots);
    let global_failures = if quota_available == 0 {
        vec![WorkerPlacementGlobalFailure::ScopeQuotaExhausted {
            max_running_slots: quota.max_running_slots,
            running_slots: quota.running_slots,
        }]
    } else {
        Vec::new()
    };

    let (affinity_candidate, affinity_failure) =
        evaluate_affinity(request, candidates, remaining_capacity, constraints);
    let selected = if global_failures.is_empty() {
        affinity_candidate.or_else(|| {
            candidates
                .iter()
                .filter(|candidate| {
                    candidate_failures(request, candidate, remaining_capacity, constraints)
                        .is_empty()
                })
                .max_by(|left, right| placement_order(left, right, remaining_capacity))
        })
    } else {
        None
    };

    if let Some(selected) = selected {
        let key = worker_key(selected);
        let worker_available_slots_after = remaining_capacity
            .get(&key)
            .copied()
            .ok_or_else(|| WorkerPlacementError::invalid("selected Worker capacity is missing"))?
            .checked_sub(1)
            .ok_or_else(|| WorkerPlacementError::invalid("selected Worker capacity underflow"))?;
        remaining_capacity.insert(key, worker_available_slots_after);
        let state = quota_states
            .get_mut(&scope_key)
            .ok_or_else(|| WorkerPlacementError::invalid("selected scope quota is missing"))?;
        state.running_slots = state
            .running_slots
            .checked_add(1)
            .ok_or_else(|| WorkerPlacementError::invalid("scope quota overflow"))?;
        let scope_available_slots_after =
            state.max_running_slots.saturating_sub(state.running_slots);
        let reused_affinity = request.affinity.as_ref().is_some_and(|affinity| {
            affinity_scope_matches(affinity, request)
                && affinity.worker_id == selected.worker.worker_id
                && affinity.worker_instance_id == selected.worker.worker_instance_id
        });
        return Ok(WorkerPlacementDecision::Selected(
            WorkerPlacementSelection {
                job_id: request.job_id.clone(),
                worker_id: selected.worker.worker_id.clone(),
                worker_instance_id: selected.worker.worker_instance_id.clone(),
                reused_affinity,
                affinity_failure,
                worker_available_slots_after,
                scope_available_slots_after,
            },
        ));
    }

    let mut workers = candidates
        .iter()
        .map(|candidate| WorkerPlacementCandidateRejection {
            worker_id: candidate.worker.worker_id.clone(),
            worker_instance_id: candidate.worker.worker_instance_id.clone(),
            failures: candidate_failures(request, candidate, remaining_capacity, constraints),
        })
        .collect::<Vec<_>>();
    workers.sort_unstable_by(|left, right| {
        left.worker_id
            .0
            .cmp(&right.worker_id.0)
            .then_with(|| left.worker_instance_id.0.cmp(&right.worker_instance_id.0))
    });
    Ok(WorkerPlacementDecision::Rejected(
        WorkerPlacementRejection {
            job_id: request.job_id.clone(),
            affinity_failure,
            global_failures,
            workers,
        },
    ))
}

fn evaluate_affinity<'candidate>(
    request: &WorkerPlacementRequest,
    candidates: &'candidate [WorkerPlacementCandidate],
    remaining_capacity: &HashMap<(String, String), u64>,
    constraints: &dyn WorkerPlacementConstraintEvaluator,
) -> (
    Option<&'candidate WorkerPlacementCandidate>,
    Option<WorkerAffinityFailure>,
) {
    let Some(affinity) = request.affinity.as_ref() else {
        return (None, None);
    };
    if !affinity_scope_matches(affinity, request) {
        return (None, Some(WorkerAffinityFailure::ScopeMismatch));
    }
    if let Some(candidate) = candidates.iter().find(|candidate| {
        candidate.worker.worker_id == affinity.worker_id
            && candidate.worker.worker_instance_id == affinity.worker_instance_id
    }) {
        let failures = candidate_failures(request, candidate, remaining_capacity, constraints);
        return if failures.is_empty() {
            (Some(candidate), None)
        } else {
            (
                None,
                Some(WorkerAffinityFailure::WorkerIneligible { failures }),
            )
        };
    }
    if let Some(current) = candidates
        .iter()
        .find(|candidate| candidate.worker.worker_id == affinity.worker_id)
    {
        return (
            None,
            Some(WorkerAffinityFailure::WorkerInstanceReplaced {
                current_worker_instance_id: current.worker.worker_instance_id.clone(),
            }),
        );
    }
    (None, Some(WorkerAffinityFailure::WorkerUnavailable))
}

fn candidate_failures(
    request: &WorkerPlacementRequest,
    candidate: &WorkerPlacementCandidate,
    remaining_capacity: &HashMap<(String, String), u64>,
    constraints: &dyn WorkerPlacementConstraintEvaluator,
) -> Vec<WorkerPlacementFailure> {
    let mut failures = Vec::new();
    if candidate.worker.protocol_version != request.protocol_version {
        failures.push(WorkerPlacementFailure::ProtocolVersionMismatch {
            required: request.protocol_version.clone(),
            actual: candidate.worker.protocol_version.clone(),
        });
    }
    if candidate.worker.platform != request.platform {
        failures.push(WorkerPlacementFailure::PlatformMismatch {
            required: request.platform,
            actual: candidate.worker.platform,
        });
    }
    let mut missing_capabilities = request
        .required_capabilities
        .iter()
        .filter(|required| !candidate.worker.capabilities.contains(required))
        .cloned()
        .collect::<Vec<_>>();
    missing_capabilities.sort_unstable();
    missing_capabilities.dedup();
    failures.extend(
        missing_capabilities
            .into_iter()
            .map(WorkerPlacementFailure::MissingCapability),
    );
    if candidate.worker.health != WorkerHealth::Healthy {
        failures.push(WorkerPlacementFailure::WorkerNotHealthy(
            candidate.worker.health,
        ));
    }
    if candidate.worker.security_zone != request.security_zone {
        failures.push(WorkerPlacementFailure::SecurityZoneMismatch {
            required: request.security_zone.clone(),
            actual: candidate.worker.security_zone.clone(),
        });
    }
    if !candidate.network_zones.contains(&request.network_zone) {
        failures.push(WorkerPlacementFailure::NetworkZoneUnavailable(
            request.network_zone.clone(),
        ));
    }
    if !candidate
        .repository_access
        .iter()
        .any(|access| repository_access_matches(access, request))
    {
        if candidate
            .repository_access
            .iter()
            .any(|access| access.repository_id == request.repository_id)
        {
            failures.push(WorkerPlacementFailure::WorkspaceTenantMismatch {
                organization_id: request.organization_id.clone(),
                workspace_id: request.workspace_id.clone(),
            });
        } else {
            failures.push(WorkerPlacementFailure::RepositoryUnreachable(
                request.repository_id.clone(),
            ));
        }
    }
    if remaining_capacity
        .get(&worker_key(candidate))
        .copied()
        .unwrap_or_default()
        == 0
    {
        failures.push(WorkerPlacementFailure::NoAvailableCapacity);
    }
    failures.extend(constraints.additional_failures(request, candidate));
    failures
}

fn placement_order(
    left: &WorkerPlacementCandidate,
    right: &WorkerPlacementCandidate,
    remaining_capacity: &HashMap<(String, String), u64>,
) -> Ordering {
    let left_capacity = remaining_capacity
        .get(&worker_key(left))
        .copied()
        .unwrap_or_default();
    let right_capacity = remaining_capacity
        .get(&worker_key(right))
        .copied()
        .unwrap_or_default();
    left_capacity
        .cmp(&right_capacity)
        .then_with(|| right.worker.worker_id.0.cmp(&left.worker.worker_id.0))
        .then_with(|| {
            right
                .worker
                .worker_instance_id
                .0
                .cmp(&left.worker.worker_instance_id.0)
        })
}

fn affinity_scope_matches(
    affinity: &WorkerSessionAffinity,
    request: &WorkerPlacementRequest,
) -> bool {
    affinity.organization_id == request.organization_id
        && affinity.workspace_id == request.workspace_id
        && affinity.project_id == request.project_id
        && affinity.repository_id == request.repository_id
        && affinity.product_session_id == request.product_session_id
}

fn repository_access_matches(
    access: &WorkerRepositoryAccess,
    request: &WorkerPlacementRequest,
) -> bool {
    access.organization_id == request.organization_id
        && access.workspace_id == request.workspace_id
        && access.project_id == request.project_id
        && access.repository_id == request.repository_id
}

fn worker_key(candidate: &WorkerPlacementCandidate) -> (String, String) {
    (
        candidate.worker.worker_id.0.clone(),
        candidate.worker.worker_instance_id.0.clone(),
    )
}

fn validate_inputs(
    requests: &[WorkerPlacementRequest],
    candidates: &[WorkerPlacementCandidate],
    quotas: &[WorkerPlacementQuota],
) -> Result<(), WorkerPlacementError> {
    let mut job_ids = HashSet::new();
    for request in requests {
        if !job_ids.insert(request.job_id.0.as_str()) {
            return Err(WorkerPlacementError::invalid(
                "placement requests contain a duplicate execution job",
            ));
        }
        if request.job_id.0.is_empty()
            || request.protocol_version.is_empty()
            || request.network_zone.is_empty()
            || request.security_zone.is_empty()
            || request.required_capabilities.iter().any(String::is_empty)
        {
            return Err(WorkerPlacementError::invalid(
                "placement requirements must not be empty",
            ));
        }
    }

    let mut worker_ids = HashSet::new();
    for candidate in candidates {
        if !worker_ids.insert(candidate.worker.worker_id.0.as_str()) {
            return Err(WorkerPlacementError::invalid(
                "capacity snapshot contains duplicate current Worker identities",
            ));
        }
        if candidate.worker.max_slots == 0
            || candidate
                .worker
                .running_slots
                .checked_add(candidate.worker.available_slots)
                != Some(candidate.worker.max_slots)
            || candidate.worker.protocol_version.is_empty()
            || candidate.worker.security_zone.is_empty()
            || candidate.network_zones.iter().any(String::is_empty)
        {
            return Err(WorkerPlacementError::invalid(
                "capacity snapshot contains invalid Worker facts",
            ));
        }
    }

    let mut quota_keys = HashSet::new();
    for quota in quotas {
        if quota.running_slots > quota.max_running_slots {
            return Err(WorkerPlacementError::invalid(
                "scope quota running slots exceed its limit",
            ));
        }
        if !quota_keys.insert(PlacementScopeKey::quota(quota)) {
            return Err(WorkerPlacementError::invalid(
                "placement snapshot contains duplicate scope quotas",
            ));
        }
    }
    if requests
        .iter()
        .any(|request| !quota_keys.contains(&PlacementScopeKey::request(request)))
    {
        return Err(WorkerPlacementError::invalid(
            "a placement request has no exact-scope quota",
        ));
    }
    Ok(())
}
