// SPDX-License-Identifier: Apache-2.0

//! Worker registration profile, health, and capacity snapshot contracts.
//!
//! Durable identity, heartbeat ordering, and lease/fence authority remain in
//! [`crate::ExecutionRegistry`]. This module defines the additional Worker
//! facts and deterministic decisions consumed by that single registry.

use serde::{Deserialize, Serialize};
use winwincode_domain::{
    Instant, OrganizationId, ProjectId, RepositoryId, Sha256Digest, WorkerId, WorkerInstanceId,
    WorkspaceId,
};

pub const EXECUTION_PROTOCOL_VERSION: &str = "winwincode/v1";

/// Exact tenant scope that owns one registered Worker.
///
/// Local Workers use the canonical local repository scope. Remote Fleet
/// adapters may register against a broader scope without creating another
/// Worker Registry authority.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkerRegistryScope {
    Organization {
        organization_id: OrganizationId,
    },
    Workspace {
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
    },
    Project {
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
    },
    Repository {
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        repository_id: RepositoryId,
    },
}

impl WorkerRegistryScope {
    /// Canonical scope of the embedded Community Worker.
    #[must_use]
    pub fn local_default() -> Self {
        Self::Repository {
            organization_id: OrganizationId("org_00000000000000000000000000".to_owned()),
            workspace_id: WorkspaceId("wsp_00000000000000000000000000".to_owned()),
            project_id: ProjectId("prj_00000000000000000000000000".to_owned()),
            repository_id: RepositoryId("rep_00000000000000000000000000".to_owned()),
        }
    }
}

/// Durable operator-controlled placement state.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerManagementState {
    Enabled,
    Draining,
}

/// Public operational state derived from management and health authority.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerOperationalState {
    Enabled,
    Draining,
    Offline,
}

/// Secret-free snapshot used by the Control Plane projection adapter.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkerManagementSnapshot {
    pub worker_id: WorkerId,
    pub scope: WorkerRegistryScope,
    pub management_state: WorkerManagementState,
    pub operational_state: WorkerOperationalState,
    pub health: WorkerHealth,
    pub revision: u64,
    pub capacity: u64,
    pub available_capacity: u64,
    pub active_lease_count: u64,
    pub last_heartbeat_at: Option<Instant>,
    pub observed_at: Instant,
}

impl WorkerManagementSnapshot {
    /// Closed public state for generated `WorkerProjection` mapping.
    #[must_use]
    pub const fn operational_state(&self) -> WorkerOperationalState {
        self.operational_state
    }
}

/// Stable cursor over one scope-local Worker page.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkerManagementPageCursor {
    pub worker_id: WorkerId,
    pub upper_bound_worker_id: WorkerId,
}

/// Stable scope-local Worker page.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkerManagementPage {
    pub workers: Vec<WorkerManagementSnapshot>,
    pub next_cursor: Option<WorkerManagementPageCursor>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind", deny_unknown_fields)]
pub enum WorkerAuthenticationIdentity {
    LocalEmbedded {
        control_plane_principal: String,
    },
    TransportPrincipal {
        issuer: String,
        subject: String,
        credential_fingerprint: Sha256Digest,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum WorkerPlatform {
    #[serde(rename = "aarch64-apple-darwin")]
    Aarch64AppleDarwin,
    #[serde(rename = "x86_64-apple-darwin")]
    X86_64AppleDarwin,
    #[serde(rename = "aarch64-unknown-linux-gnu")]
    Aarch64UnknownLinuxGnu,
    #[serde(rename = "x86_64-unknown-linux-gnu")]
    X86_64UnknownLinuxGnu,
}

impl WorkerPlatform {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Aarch64AppleDarwin => "aarch64-apple-darwin",
            Self::X86_64AppleDarwin => "x86_64-apple-darwin",
            Self::Aarch64UnknownLinuxGnu => "aarch64-unknown-linux-gnu",
            Self::X86_64UnknownLinuxGnu => "x86_64-unknown-linux-gnu",
        }
    }

    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value {
            "aarch64-apple-darwin" => Some(Self::Aarch64AppleDarwin),
            "x86_64-apple-darwin" => Some(Self::X86_64AppleDarwin),
            "aarch64-unknown-linux-gnu" => Some(Self::Aarch64UnknownLinuxGnu),
            "x86_64-unknown-linux-gnu" => Some(Self::X86_64UnknownLinuxGnu),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerHealth {
    Registered,
    Healthy,
    TimedOut,
}

impl WorkerHealth {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Registered => "registered",
            Self::Healthy => "healthy",
            Self::TimedOut => "timed_out",
        }
    }

    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value {
            "registered" => Some(Self::Registered),
            "healthy" => Some(Self::Healthy),
            "timed_out" => Some(Self::TimedOut),
            _ => None,
        }
    }
}

/// Stable reason attached to a rejected registration.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerRegistrationErrorCode {
    AuthenticationMismatch,
    ProtocolVersionUnsupported,
    CapabilityMismatch,
    SecurityZoneMismatch,
    ScopeMismatch,
    MessageConflict,
}

/// Current capacity for one canonical Worker process instance.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkerCapacityEntry {
    pub worker_id: WorkerId,
    pub worker_instance_id: WorkerInstanceId,
    pub protocol_version: String,
    pub platform: WorkerPlatform,
    pub capabilities: Vec<String>,
    pub security_zone: String,
    pub health: WorkerHealth,
    pub max_slots: u64,
    pub running_slots: u64,
    pub available_slots: u64,
}

/// One transactionally consistent view across all current Worker instances.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkerCapacitySnapshot {
    pub observed_at: Instant,
    pub workers: Vec<WorkerCapacityEntry>,
    pub healthy_max_slots: u64,
    pub healthy_running_slots: u64,
    pub healthy_available_slots: u64,
}

impl WorkerCapacitySnapshot {
    #[must_use]
    pub fn healthy_worker_count(&self) -> usize {
        self.workers
            .iter()
            .filter(|worker| worker.health == WorkerHealth::Healthy)
            .count()
    }
}
