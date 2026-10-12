// SPDX-License-Identifier: Apache-2.0

use serde::{Deserialize, Serialize};
use winwincode_domain::{
    Instant, OrganizationId, ProjectId, RepositoryId, Sha256Digest, WorkerId, WorkerInstanceId,
    WorkspaceId,
};

pub const EXECUTION_PROTOCOL_VERSION: &str = "winwincode/v1;usage=2";

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

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerManagementState {
    Enabled,
    Draining,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerOperationalState {
    Enabled,
    Draining,
    Offline,
}

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
    #[must_use]
    pub const fn operational_state(&self) -> WorkerOperationalState {
        self.operational_state
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkerManagementPageCursor {
    pub worker_id: WorkerId,
    pub upper_bound_worker_id: WorkerId,
}

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
            .filter(|worker| {
                worker.health == WorkerHealth::Healthy
                    && worker.protocol_version == EXECUTION_PROTOCOL_VERSION
            })
            .count()
    }
}
