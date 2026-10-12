#![allow(clippy::struct_field_names)]
// SPDX-License-Identifier: Apache-2.0

use serde::Deserialize;
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PresenceState {
    PendingEnrollment,
    Online,
    Degraded,
    Offline,
    Locked,
    Revoked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientLockState {
    Unlocked,
    Locked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectCodeState {
    Active,
    Consumed,
    Expired,
    Revoked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientAccessPermission {
    Use,
    Manage,
    Share,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientTrustMode {
    Temporary,
    Trusted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientAccessGrantState {
    Active,
    Revoked,
    Expired,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientAccessGrantSource {
    ConnectCode,
    Administrator,
    LocalConfirmation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OccupancyLeaseState {
    Available,
    Reserving,
    Occupied,
    Draining,
    RecoveryPending,
    Released,
    Expired,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepositoryKind {
    Git,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepositoryDirtyState {
    Clean,
    Dirty,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepositoryAvailability {
    Available,
    Dirty,
    Unavailable,
    Moved,
    InvalidGit,
    PermissionDenied,
    ScanFailed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepositoryAccessPermission {
    Use,
    Manage,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepositoryAccessGrantState {
    Active,
    Revoked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerLaunchGrantState {
    Issued,
    Consumed,
    Revoked,
    Expired,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LocalCandidateState {
    Retained,
    BranchCreated,
    Applied,
    Discarded,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApplyStrategy {
    CreateBranch,
    FastForward,
    CherryPick,
    Merge,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApplyResult {
    Retained,
    BranchCreated,
    Applied,
    BaseStale,
    WorkingTreeDirty,
    MergeConflict,
    CandidateMissing,
    PermissionDenied,
    Discarded,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ClientPlatformTarget {
    #[serde(rename = "aarch64-apple-darwin")]
    Aarch64AppleDarwin,
    #[serde(rename = "x86_64-apple-darwin")]
    X8664AppleDarwin,
    #[serde(rename = "aarch64-unknown-linux-gnu")]
    Aarch64UnknownLinuxGnu,
    #[serde(rename = "x86_64-unknown-linux-gnu")]
    X8664UnknownLinuxGnu,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ClientArchitecture {
    #[serde(rename = "aarch64")]
    Aarch64,
    #[serde(rename = "x86_64")]
    X8664,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OccupancyRejectReason {
    UnknownLease,
    StaleFencingToken,
    LocalStateConflict,
    ClientLocked,
    CapacityExhausted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientOccupancyReleaseMode {
    Immediate,
    DrainThenRelease,
    CancelTasksAndRelease,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientOccupancyReleaseReason {
    UserRequested,
    CancelTasksAndRelease,
    Drained,
    IdleTimeout,
    Revoked,
    ForceFenced,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientOccupancyForceFenceReason {
    RecoveryDeadlineExceeded,
    AdministratorForceClean,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientRepositoryRescanReason {
    OccupantRequested,
    OccupancyRecovered,
    Policy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientWorkerStopReason {
    OccupantRequested,
    DrainingComplete,
    LeaseRecovered,
    GrantRevoked,
    Superseded,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientWorkerRunState {
    Starting,
    Running,
    Draining,
    Stopping,
    Stopped,
    Crashed,
    Missing,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerReconcileState {
    StillRunning,
    Terminal,
    Missing,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerLaunchAckStatus {
    Accepted,
    Duplicate,
    RejectedStaleFencingToken,
    RejectedLeaseMismatch,
    RejectedCapacityExhausted,
    RejectedRepositoryUnavailable,
    RejectedUnknownGrant,
    RejectedWrongState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandAckStatus {
    Accepted,
    Duplicate,
    RejectedUnknownCommand,
    RejectedRevisionConflict,
    RejectedStaleFencingToken,
    RejectedLeaseMismatch,
    RejectedWrongState,
    RejectedUnauthorized,
    RejectedCapacityExhausted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ClientControlErrorCode {
    ProtocolVersionUnsupported,
    DeviceNotEnrolled,
    DeviceInstanceChanged,
    UnknownLease,
    StaleFencingToken,
    RevisionConflict,
    IdempotencyConflict,
    CapacityExhausted,
    RepositoryUnavailable,
    GrantInvalid,
    WrongState,
    RateLimited,
    InternalError,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ClientControlMessageKind {
    #[serde(rename = "client.extension.report")]
    ExtensionReport,
    #[serde(rename = "client.repository.register")]
    RepositoryRegister,
    #[serde(rename = "client.repository.registered")]
    RepositoryRegistered,
    #[serde(rename = "client.extension.apply")]
    ExtensionApply,
    #[serde(rename = "client.provider.report")]
    ProviderReport,
    #[serde(rename = "client.provider.apply")]
    ProviderApply,
    #[serde(rename = "client.enroll")]
    Enroll,
    #[serde(rename = "client.hello")]
    Hello,
    #[serde(rename = "client.heartbeat")]
    Heartbeat,
    #[serde(rename = "client.connect_code.published")]
    ConnectCodePublished,
    #[serde(rename = "client.occupancy.ack")]
    OccupancyAck,
    #[serde(rename = "client.occupancy.rejected")]
    OccupancyRejected,
    #[serde(rename = "client.repository.upsert")]
    RepositoryUpsert,
    #[serde(rename = "client.repository.removed")]
    RepositoryRemoved,
    #[serde(rename = "client.repository.status")]
    RepositoryStatus,
    #[serde(rename = "client.worker.launch_ack")]
    WorkerLaunchAck,
    #[serde(rename = "client.worker.state")]
    WorkerState,
    #[serde(rename = "client.worker.reconcile")]
    WorkerReconcile,
    #[serde(rename = "client.candidate.retained")]
    CandidateRetained,
    #[serde(rename = "client.candidate.apply_result")]
    CandidateApplyResult,
    #[serde(rename = "client.command_ack")]
    CommandAck,
    #[serde(rename = "client.occupancy.offer")]
    OccupancyOffer,
    #[serde(rename = "client.occupancy.release")]
    OccupancyRelease,
    #[serde(rename = "client.occupancy.force_fence")]
    OccupancyForceFence,
    #[serde(rename = "client.repository.rescan")]
    RepositoryRescan,
    #[serde(rename = "client.worker.launch")]
    WorkerLaunch,
    #[serde(rename = "client.worker.stop")]
    WorkerStop,
    #[serde(rename = "client.candidate.apply")]
    CandidateApply,
    #[serde(rename = "client.client_lock")]
    ClientLock,
    #[serde(rename = "client.managed_app.command")]
    ManagedAppCommand,
    #[serde(rename = "client.managed_app.status")]
    ManagedAppStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientNode {
    #[serde(rename = "clientNodeId")]
    pub client_node_id: String,
    #[serde(rename = "publicClientId")]
    pub public_client_id: String,
    #[serde(rename = "displayName")]
    pub display_name: String,
    pub platform: String,
    pub architecture: String,
    #[serde(rename = "clientVersion")]
    pub client_version: String,
    #[serde(rename = "deviceCredentialDigest")]
    pub device_credential_digest: Option<String>,
    #[serde(rename = "currentInstanceId")]
    pub current_instance_id: Option<String>,
    #[serde(rename = "presenceState")]
    pub presence_state: PresenceState,
    #[serde(rename = "acceptingConnections")]
    pub accepting_connections: bool,
    #[serde(rename = "lockState")]
    pub lock_state: ClientLockState,
    #[serde(rename = "maxConcurrentWorkerSessions")]
    pub max_concurrent_worker_sessions: u32,
    #[serde(rename = "reportedRunningWorkerSessions")]
    pub reported_running_worker_sessions: u32,
    #[serde(rename = "lastHeartbeatAt")]
    pub last_heartbeat_at: Option<String>,
    #[serde(rename = "createdAt")]
    pub created_at: String,
    pub revision: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientConnectCode {
    #[serde(rename = "connectCodeId")]
    pub connect_code_id: String,
    #[serde(rename = "clientNodeId")]
    pub client_node_id: String,
    #[serde(rename = "codeDigest")]
    pub code_digest: String,
    #[serde(rename = "issuedByInstanceId")]
    pub issued_by_instance_id: String,
    #[serde(rename = "expiresAt")]
    pub expires_at: String,
    #[serde(rename = "remainingAttempts")]
    pub remaining_attempts: u32,
    pub state: ConnectCodeState,
    #[serde(rename = "createdAt")]
    pub created_at: String,
    pub revision: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientAccessGrant {
    #[serde(rename = "clientAccessGrantId")]
    pub client_access_grant_id: String,
    #[serde(rename = "clientNodeId")]
    pub client_node_id: String,
    #[serde(rename = "userId")]
    pub user_id: String,
    pub permissions: Vec<ClientAccessPermission>,
    #[serde(rename = "trustMode")]
    pub trust_mode: ClientTrustMode,
    pub state: ClientAccessGrantState,
    #[serde(rename = "grantedByUserId")]
    pub granted_by_user_id: String,
    #[serde(rename = "grantSource")]
    pub grant_source: ClientAccessGrantSource,
    #[serde(rename = "expiresAt")]
    pub expires_at: Option<String>,
    #[serde(rename = "createdAt")]
    pub created_at: String,
    pub revision: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientCapacityReport {
    #[serde(rename = "maxConcurrentWorkerSessions")]
    pub max_concurrent_worker_sessions: u32,
    #[serde(rename = "runningWorkerSessions")]
    pub running_worker_sessions: u32,
    #[serde(rename = "reservedWorkerSessions")]
    pub reserved_worker_sessions: u32,
    #[serde(rename = "drainingWorkerSessions")]
    pub draining_worker_sessions: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepositoryBindingProjection {
    #[serde(rename = "repositoryBindingId")]
    pub repository_binding_id: String,
    #[serde(rename = "displayName")]
    pub display_name: String,
    #[serde(rename = "repositoryKind")]
    pub repository_kind: RepositoryKind,
    #[serde(rename = "defaultBranch")]
    pub default_branch: String,
    #[serde(rename = "headCommit")]
    pub head_commit: String,
    #[serde(rename = "dirtyState")]
    pub dirty_state: RepositoryDirtyState,
    pub availability: RepositoryAvailability,
    #[serde(rename = "repositoryFingerprint")]
    pub repository_fingerprint: String,
    #[serde(rename = "lastScannedAt")]
    pub last_scanned_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientControlError {
    pub code: ClientControlErrorCode,
    pub message: String,
    pub retryable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientOccupancyLease {
    #[serde(rename = "clientOccupancyLeaseId")]
    pub client_occupancy_lease_id: String,
    #[serde(rename = "clientNodeId")]
    pub client_node_id: String,
    #[serde(rename = "holderUserId")]
    pub holder_user_id: String,
    pub state: OccupancyLeaseState,
    #[serde(rename = "fencingToken", with = "crate::wire::fencing_token")]
    pub fencing_token: u64,
    #[serde(rename = "claimRequestId")]
    pub claim_request_id: String,
    #[serde(rename = "claimedAt")]
    pub claimed_at: String,
    #[serde(rename = "acknowledgedAt")]
    pub acknowledged_at: Option<String>,
    #[serde(rename = "lastRenewedAt")]
    pub last_renewed_at: Option<String>,
    #[serde(rename = "idleExpiresAt")]
    pub idle_expires_at: Option<String>,
    #[serde(rename = "recoveryDeadlineAt")]
    pub recovery_deadline_at: Option<String>,
    #[serde(rename = "releaseRequestedAt")]
    pub release_requested_at: Option<String>,
    #[serde(rename = "releasedAt")]
    pub released_at: Option<String>,
    #[serde(rename = "releaseReason")]
    pub release_reason: Option<ClientOccupancyReleaseReason>,
    pub revision: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepositoryBinding {
    #[serde(rename = "repositoryBindingId")]
    pub repository_binding_id: String,
    #[serde(rename = "clientNodeId")]
    pub client_node_id: String,
    #[serde(rename = "displayName")]
    pub display_name: String,
    #[serde(rename = "repositoryKind")]
    pub repository_kind: RepositoryKind,
    #[serde(rename = "defaultBranch")]
    pub default_branch: String,
    #[serde(rename = "headCommit")]
    pub head_commit: String,
    #[serde(rename = "dirtyState")]
    pub dirty_state: RepositoryDirtyState,
    pub availability: RepositoryAvailability,
    #[serde(rename = "repositoryFingerprint")]
    pub repository_fingerprint: String,
    #[serde(rename = "lastScannedAt")]
    pub last_scanned_at: Option<String>,
    pub revision: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepositoryAccessGrant {
    #[serde(rename = "repositoryAccessGrantId")]
    pub repository_access_grant_id: String,
    #[serde(rename = "repositoryBindingId")]
    pub repository_binding_id: String,
    #[serde(rename = "userId")]
    pub user_id: String,
    pub permissions: Vec<RepositoryAccessPermission>,
    pub state: RepositoryAccessGrantState,
    #[serde(rename = "grantedByUserId")]
    pub granted_by_user_id: String,
    #[serde(rename = "createdAt")]
    pub created_at: String,
    pub revision: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerLaunchGrant {
    #[serde(rename = "workerLaunchGrantId")]
    pub worker_launch_grant_id: String,
    #[serde(rename = "clientNodeId")]
    pub client_node_id: String,
    #[serde(rename = "clientInstanceId")]
    pub client_instance_id: String,
    #[serde(rename = "occupancyLeaseId")]
    pub occupancy_lease_id: String,
    #[serde(rename = "occupancyFencingToken", with = "crate::wire::fencing_token")]
    pub occupancy_fencing_token: u64,
    #[serde(rename = "repositoryBindingId")]
    pub repository_binding_id: String,
    #[serde(rename = "productSessionId")]
    pub product_session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "workRunId")]
    pub work_run_id: Option<String>,
    #[serde(rename = "workerSessionId")]
    pub worker_session_id: String,
    #[serde(rename = "workerId")]
    pub worker_id: String,
    #[serde(rename = "workerInstanceId")]
    pub worker_instance_id: String,
    #[serde(rename = "credentialDigest")]
    pub credential_digest: String,
    #[serde(rename = "expiresAt")]
    pub expires_at: String,
    pub state: WorkerLaunchGrantState,
    pub revision: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalCandidateReceipt {
    #[serde(rename = "localCandidateReceiptId")]
    pub local_candidate_receipt_id: String,
    #[serde(rename = "candidateRef")]
    pub candidate_ref: String,
    #[serde(rename = "repositoryBindingId")]
    pub repository_binding_id: String,
    #[serde(rename = "candidateCommit")]
    pub candidate_commit: String,
    #[serde(rename = "localRefName")]
    pub local_ref_name: String,
    pub state: LocalCandidateState,
    #[serde(rename = "createdAt")]
    pub created_at: String,
    pub revision: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalApplyReceipt {
    #[serde(rename = "localApplyReceiptId")]
    pub local_apply_receipt_id: String,
    #[serde(rename = "candidateRef")]
    pub candidate_ref: String,
    #[serde(rename = "repositoryBindingId")]
    pub repository_binding_id: String,
    #[serde(rename = "targetBranch")]
    pub target_branch: String,
    #[serde(rename = "expectedHead")]
    pub expected_head: String,
    pub strategy: ApplyStrategy,
    pub result: ApplyResult,
    #[serde(rename = "resultingCommit")]
    pub resulting_commit: Option<String>,
    #[serde(rename = "conflictArtifactRef")]
    pub conflict_artifact_ref: Option<String>,
    #[serde(rename = "createdAt")]
    pub created_at: String,
    pub revision: u64,
}

#[cfg(test)]
mod tests {
    use serde::de::DeserializeOwned;

    use super::*;

    fn assert_round_trip<T>(json: &str) -> T
    where
        T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug,
    {
        let parsed: T = serde_json::from_str(json).expect("deserialize inline JSON");
        let serialized = serde_json::to_string(&parsed).expect("serialize value");
        let reparsed: T = serde_json::from_str(&serialized).expect("deserialize reserialized JSON");
        assert_eq!(
            parsed, reparsed,
            "reserialized struct must be semantically equal"
        );
        let expected: serde_json::Value =
            serde_json::from_str(json).expect("parse inline JSON as a value");
        assert_eq!(
            serde_json::to_value(&reparsed).expect("serialize reserialized to a value"),
            expected,
            "reserialized JSON must be semantically equal to the inline JSON"
        );
        parsed
    }

    #[test]
    fn client_node_round_trips() {
        assert_round_trip::<ClientNode>(
            r#"{
                "clientNodeId": "node_01j2",
                "publicClientId": "pub_dev_99",
                "displayName": "Cheng's MacBook",
                "platform": "darwin",
                "architecture": "arm64",
                "clientVersion": "0.1.0-alpha.1",
                "deviceCredentialDigest": "sha256:aa11",
                "currentInstanceId": "inst_01j2",
                "presenceState": "online",
                "acceptingConnections": true,
                "lockState": "unlocked",
                "maxConcurrentWorkerSessions": 3,
                "reportedRunningWorkerSessions": 1,
                "lastHeartbeatAt": "2026-01-02T12:00:00Z",
                "createdAt": "2026-01-01T00:00:00Z",
                "revision": 41
            }"#,
        );
    }

    #[test]
    fn client_connect_code_round_trips() {
        assert_round_trip::<ClientConnectCode>(
            r#"{
                "connectCodeId": "code_01j2",
                "clientNodeId": "node_01j2",
                "codeDigest": "sha256:bb22",
                "issuedByInstanceId": "inst_01j2",
                "expiresAt": "2026-01-01T01:00:00Z",
                "remainingAttempts": 5,
                "state": "active",
                "createdAt": "2026-01-01T00:00:00Z",
                "revision": 1
            }"#,
        );
    }

    #[test]
    fn client_access_grant_round_trips() {
        assert_round_trip::<ClientAccessGrant>(
            r#"{
                "clientAccessGrantId": "cag_01j2",
                "clientNodeId": "node_01j2",
                "userId": "usr_01j2",
                "permissions": ["use", "manage", "share"],
                "trustMode": "trusted",
                "state": "active",
                "grantedByUserId": "usr_01j2",
                "grantSource": "connect_code",
                "expiresAt": null,
                "createdAt": "2026-01-01T00:00:00Z",
                "revision": 2
            }"#,
        );
    }

    #[test]
    fn client_capacity_report_round_trips() {
        assert_round_trip::<ClientCapacityReport>(
            r#"{
                "maxConcurrentWorkerSessions": 3,
                "runningWorkerSessions": 1,
                "reservedWorkerSessions": 1,
                "drainingWorkerSessions": 0
            }"#,
        );
    }

    #[test]
    fn repository_binding_projection_round_trips() {
        assert_round_trip::<RepositoryBindingProjection>(
            r#"{
                "repositoryBindingId": "rbd_01j2",
                "displayName": "winwincode",
                "repositoryKind": "git",
                "defaultBranch": "main",
                "headCommit": "0123456789abcdef0123456789abcdef01234567",
                "dirtyState": "clean",
                "availability": "available",
                "repositoryFingerprint": "sha256:cc33",
                "lastScannedAt": "2026-01-02T11:00:00.000Z"
            }"#,
        );
    }

    #[test]
    fn client_control_error_round_trips() {
        assert_round_trip::<ClientControlError>(
            r#"{
                "code": "STALE_FENCING_TOKEN",
                "message": "a newer occupancy superseded the lease",
                "retryable": false
            }"#,
        );
    }

    #[test]
    fn client_occupancy_lease_round_trips() {
        assert_round_trip::<ClientOccupancyLease>(
            r#"{
                "clientOccupancyLeaseId": "lease_01j2",
                "clientNodeId": "node_01j2",
                "holderUserId": "usr_01j2",
                "state": "occupied",
                "fencingToken": "7",
                "claimRequestId": "claim_01j2",
                "claimedAt": "2026-01-02T12:00:00.000Z",
                "acknowledgedAt": "2026-01-02T12:00:01.000Z",
                "lastRenewedAt": "2026-01-02T12:05:00.000Z",
                "idleExpiresAt": null,
                "recoveryDeadlineAt": null,
                "releaseRequestedAt": null,
                "releasedAt": null,
                "releaseReason": null,
                "revision": 9
            }"#,
        );
    }

    #[test]
    fn occupancy_lease_release_reason_round_trips() {
        let parsed: ClientOccupancyLease = assert_round_trip::<ClientOccupancyLease>(
            r#"{
                "clientOccupancyLeaseId": "lease_01j2",
                "clientNodeId": "node_01j2",
                "holderUserId": "usr_01j2",
                "state": "released",
                "fencingToken": "18446744073709551615",
                "claimRequestId": "claim_01j2",
                "claimedAt": "2026-01-02T12:00:00.000Z",
                "acknowledgedAt": null,
                "lastRenewedAt": null,
                "idleExpiresAt": null,
                "recoveryDeadlineAt": null,
                "releaseRequestedAt": "2026-01-02T13:00:00.000Z",
                "releasedAt": "2026-01-02T13:01:00.000Z",
                "releaseReason": "drained",
                "revision": 10
            }"#,
        );
        assert_eq!(parsed.fencing_token, u64::MAX);
        assert_eq!(
            parsed.release_reason,
            Some(ClientOccupancyReleaseReason::Drained)
        );
    }

    #[test]
    fn repository_binding_round_trips() {
        assert_round_trip::<RepositoryBinding>(
            r#"{
                "repositoryBindingId": "rb_01j2",
                "clientNodeId": "node_01j2",
                "displayName": "winwincode",
                "repositoryKind": "git",
                "defaultBranch": "main",
                "headCommit": "0123456789abcdef0123456789abcdef01234567",
                "dirtyState": "clean",
                "availability": "available",
                "repositoryFingerprint": "sha256:cc33",
                "lastScannedAt": "2026-01-02T11:00:00Z",
                "revision": 12
            }"#,
        );
    }

    #[test]
    fn repository_access_grant_round_trips() {
        assert_round_trip::<RepositoryAccessGrant>(
            r#"{
                "repositoryAccessGrantId": "rag_01j2",
                "repositoryBindingId": "rb_01j2",
                "userId": "usr_01j2",
                "permissions": ["use", "manage"],
                "state": "active",
                "grantedByUserId": "usr_01j2",
                "createdAt": "2026-01-01T00:00:00Z",
                "revision": 1
            }"#,
        );
    }

    #[test]
    fn worker_launch_grant_round_trips() {
        assert_round_trip::<WorkerLaunchGrant>(
            r#"{
                "workerLaunchGrantId": "wlg_01j2",
                "clientNodeId": "node_01j2",
                "clientInstanceId": "inst_01j2",
                "occupancyLeaseId": "lease_01j2",
                "occupancyFencingToken": "7",
                "repositoryBindingId": "rb_01j2",
                "productSessionId": "ps_01j2",
                "workRunId": "wrn_01j2",
                "workerSessionId": "wsn_01j2",
                "workerId": "worker_1",
                "workerInstanceId": "winst_01j2",
                "credentialDigest": "sha256:dd44",
                "expiresAt": "2026-01-02T12:10:00.000Z",
                "state": "issued",
                "revision": 1
            }"#,
        );
    }

    #[test]
    fn product_session_launch_grant_omits_work_run() {
        let grant: WorkerLaunchGrant = serde_json::from_str(
            r#"{
                "workerLaunchGrantId":"wlg_01j2","clientNodeId":"node_01j2",
                "clientInstanceId":"inst_01j2","occupancyLeaseId":"lease_01j2",
                "occupancyFencingToken":"7","repositoryBindingId":"rb_01j2",
                "productSessionId":"ps_01j2","workerSessionId":"wsn_01j2",
                "workerId":"worker_1","workerInstanceId":"winst_01j2",
                "credentialDigest":"sha256:dd44","expiresAt":"2026-01-02T12:10:00.000Z",
                "state":"issued","revision":1
            }"#,
        )
        .expect("ProductSession grant");
        assert_eq!(grant.work_run_id, None);
        assert!(
            !serde_json::to_string(&grant)
                .expect("encoded grant")
                .contains("workRunId")
        );
    }

    #[test]
    fn local_candidate_receipt_round_trips() {
        assert_round_trip::<LocalCandidateReceipt>(
            r#"{
                "localCandidateReceiptId": "lcr_01j2",
                "candidateRef": "cand_01j2",
                "repositoryBindingId": "rb_01j2",
                "candidateCommit": "89abcdef0123456789abcdef0123456789abcdef",
                "localRefName": "refs/winwincode/candidates/cand_01j2",
                "state": "retained",
                "createdAt": "2026-01-02T12:30:00Z",
                "revision": 1
            }"#,
        );
    }

    #[test]
    fn local_apply_receipt_round_trips() {
        assert_round_trip::<LocalApplyReceipt>(
            r#"{
                "localApplyReceiptId": "lar_01j2",
                "candidateRef": "cand_01j2",
                "repositoryBindingId": "rb_01j2",
                "targetBranch": "feature/x",
                "expectedHead": "0123456789abcdef0123456789abcdef01234567",
                "strategy": "cherry_pick",
                "result": "applied",
                "resultingCommit": "fedcba9876543210fedcba9876543210fedcba98",
                "conflictArtifactRef": null,
                "createdAt": "2026-01-02T12:31:00Z",
                "revision": 2
            }"#,
        );
    }
}
