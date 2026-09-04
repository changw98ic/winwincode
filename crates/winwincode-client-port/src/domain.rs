// SPDX-License-Identifier: Apache-2.0

//! Domain objects for the multi-user shared device client (plan section 7).
//!
//! Field names on the wire match the plan YAML spelling exactly. Nullable
//! plan fields are modeled as [`Option`]; every other field is required.

#![allow(clippy::struct_field_names)]

use serde::Deserialize;
use serde::Serialize;

/// Role of a user account within a WinWinCode server (plan 7.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UserRole {
    /// Server owner with administrative rights.
    Owner,
    /// Regular member.
    Member,
}

/// Lifecycle state of a user account (plan 7.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UserAccountState {
    /// Account may authenticate.
    Active,
    /// Account is disabled and may not authenticate.
    Disabled,
}

/// Machine-level presence state of a client node (plan 4.1, 7.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PresenceState {
    /// Registered but not yet enrolled.
    PendingEnrollment,
    /// Connected to the server.
    Online,
    /// Connected with degraded capability.
    Degraded,
    /// Not connected.
    Offline,
    /// Locked by an administrator.
    Locked,
    /// Revoked and not allowed to reconnect.
    Revoked,
}

/// Machine-level lock state of a client node (plan 7.2, 12.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientLockState {
    /// The node accepts new work.
    Unlocked,
    /// The node is locked and rejects new work.
    Locked,
}

/// Lifecycle state of a dynamic connect code (plan 7.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectCodeState {
    /// Usable for a first connection.
    Active,
    /// Already used for a successful connection.
    Consumed,
    /// Past its expiry.
    Expired,
    /// Withdrawn before use.
    Revoked,
}

/// Permission granted over a client node (plan 4.2, 7.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientAccessPermission {
    /// May use the client while holding occupancy.
    Use,
    /// May manage the client and its repositories.
    Manage,
    /// May grant access to other users.
    Share,
}

/// Trust mode of a client access grant (plan 7.4, 11.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientTrustMode {
    /// Grant expires and is not persisted as trusted.
    Temporary,
    /// Grant is trusted until revoked.
    Trusted,
}

/// Lifecycle state of a client access grant (plan 7.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientAccessGrantState {
    /// The grant currently authorizes the user.
    Active,
    /// The grant was revoked.
    Revoked,
    /// The grant reached its expiry.
    Expired,
}

/// Origin of a client access grant (plan 7.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientAccessGrantSource {
    /// Granted by consuming a dynamic connect code.
    ConnectCode,
    /// Granted directly by an administrator.
    Administrator,
    /// Granted by confirming on the device itself.
    LocalConfirmation,
}

/// Lifecycle state of an occupancy lease (plan 4.3, 12).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OccupancyLeaseState {
    /// No active lease on the client node.
    Available,
    /// Lease is reserving the client node.
    Reserving,
    /// Lease is acknowledged and occupies the client node.
    Occupied,
    /// Lease is draining active tasks before release.
    Draining,
    /// Holder disconnected; waiting for recovery.
    RecoveryPending,
    /// Lease was released.
    Released,
    /// Lease expired without release.
    Expired,
}

/// Kind of a registered repository binding (plan 7.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepositoryKind {
    /// A Git working copy.
    Git,
}

/// Dirty projection of a repository binding (plan 7.6).
///
/// The plan does not enumerate this value set; `clean` and `dirty` are the
/// minimal skeleton values pending contract finalization.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepositoryDirtyState {
    /// Working tree matches HEAD.
    Clean,
    /// Working tree has local modifications.
    Dirty,
}

/// Availability projection of a repository binding (plan 13.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepositoryAvailability {
    /// Scanned and usable.
    Available,
    /// Usable but with local modifications.
    Dirty,
    /// Path is not usable.
    Unavailable,
    /// Path moved since the last scan.
    Moved,
    /// Path is not a valid Git repository.
    InvalidGit,
    /// The client process cannot access the path.
    PermissionDenied,
    /// The last scan failed.
    ScanFailed,
}

/// Permission granted over a repository binding (plan 7.7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepositoryAccessPermission {
    /// May use the repository in tasks.
    Use,
    /// May manage the repository binding and grants.
    Manage,
}

/// Lifecycle state of a repository access grant (plan 7.7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepositoryAccessGrantState {
    /// The grant currently authorizes the user.
    Active,
    /// The grant was revoked.
    Revoked,
}

/// Lifecycle state of a worker launch grant (plan 7.8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerLaunchGrantState {
    /// Issued and ready to be consumed by the device client.
    Issued,
    /// Consumed by a successful worker launch.
    Consumed,
    /// Revoked before consumption.
    Revoked,
    /// Expired before consumption.
    Expired,
}

/// Lifecycle state of a locally retained candidate (plan 7.9, 15).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LocalCandidateState {
    /// Candidate ref is retained locally.
    Retained,
    /// A local branch was created from the candidate.
    BranchCreated,
    /// Candidate was applied to a target branch.
    Applied,
    /// Candidate was discarded.
    Discarded,
    /// Retention or application failed.
    Failed,
}

/// Strategy used to apply a candidate to a target branch (plan 7.10, 15.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApplyStrategy {
    /// Create a new local branch.
    CreateBranch,
    /// Fast-forward the target branch.
    FastForward,
    /// Cherry-pick onto the target branch.
    CherryPick,
    /// Merge into the target branch.
    Merge,
}

/// Terminal result of applying a candidate (plan 15.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApplyResult {
    /// Candidate remains retained.
    Retained,
    /// A branch was created from the candidate.
    BranchCreated,
    /// Candidate was applied.
    Applied,
    /// Target base moved; retry required.
    BaseStale,
    /// Target working tree was dirty.
    WorkingTreeDirty,
    /// Merge produced conflicts.
    MergeConflict,
    /// Candidate ref no longer exists.
    CandidateMissing,
    /// User lacks repository permission.
    PermissionDenied,
    /// Candidate was discarded.
    Discarded,
    /// Apply failed.
    Failed,
}

/// A user account on a WinWinCode server (plan 7.1).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UserAccount {
    /// Stable user identifier.
    #[serde(rename = "userId")]
    pub user_id: String,
    /// Display username as entered.
    pub username: String,
    /// Normalized unique username.
    #[serde(rename = "normalizedUsername")]
    pub normalized_username: String,
    /// Password verifier digest; never leaves the control plane.
    #[serde(rename = "passwordHash")]
    pub password_hash: String,
    /// Administrative role.
    pub role: UserRole,
    /// Lifecycle state.
    pub state: UserAccountState,
    /// Creation timestamp (RFC 3339).
    #[serde(rename = "createdAt")]
    pub created_at: String,
    /// Last update timestamp (RFC 3339).
    #[serde(rename = "updatedAt")]
    pub updated_at: String,
    /// Monotonic optimistic-concurrency revision.
    pub revision: u64,
}

/// A registered client node (device) projection (plan 7.2).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientNode {
    /// Stable server-side node identifier.
    #[serde(rename = "clientNodeId")]
    pub client_node_id: String,
    /// Stable public device identifier, not a secret.
    #[serde(rename = "publicClientId")]
    pub public_client_id: String,
    /// Human-readable device name.
    #[serde(rename = "displayName")]
    pub display_name: String,
    /// Operating system platform.
    pub platform: String,
    /// CPU architecture.
    pub architecture: String,
    /// Device client software version.
    #[serde(rename = "clientVersion")]
    pub client_version: String,
    /// Digest of the device credential.
    #[serde(rename = "deviceCredentialDigest")]
    pub device_credential_digest: Option<String>,
    /// Current process instance identifier.
    #[serde(rename = "currentInstanceId")]
    pub current_instance_id: Option<String>,
    /// Machine-level presence state.
    #[serde(rename = "presenceState")]
    pub presence_state: PresenceState,
    /// Whether the node accepts new connections.
    #[serde(rename = "acceptingConnections")]
    pub accepting_connections: bool,
    /// Machine-level lock state.
    #[serde(rename = "lockState")]
    pub lock_state: ClientLockState,
    /// Maximum concurrent worker sessions.
    #[serde(rename = "maxConcurrentWorkerSessions")]
    pub max_concurrent_worker_sessions: u32,
    /// Worker sessions reported running by the device.
    #[serde(rename = "reportedRunningWorkerSessions")]
    pub reported_running_worker_sessions: u32,
    /// Last heartbeat timestamp (RFC 3339).
    #[serde(rename = "lastHeartbeatAt")]
    pub last_heartbeat_at: Option<String>,
    /// Creation timestamp (RFC 3339).
    #[serde(rename = "createdAt")]
    pub created_at: String,
    /// Monotonic optimistic-concurrency revision.
    pub revision: u64,
}

/// A dynamic connect code projection (plan 7.3, 11.3).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientConnectCode {
    /// Connect code identifier.
    #[serde(rename = "connectCodeId")]
    pub connect_code_id: String,
    /// Client node the code enrolls.
    #[serde(rename = "clientNodeId")]
    pub client_node_id: String,
    /// Digest of the code; the code itself never reaches the server.
    #[serde(rename = "codeDigest")]
    pub code_digest: String,
    /// Device instance that issued the code.
    #[serde(rename = "issuedByInstanceId")]
    pub issued_by_instance_id: String,
    /// Expiry timestamp (RFC 3339).
    #[serde(rename = "expiresAt")]
    pub expires_at: String,
    /// Remaining verification attempts.
    #[serde(rename = "remainingAttempts")]
    pub remaining_attempts: u32,
    /// Lifecycle state.
    pub state: ConnectCodeState,
    /// Creation timestamp (RFC 3339).
    #[serde(rename = "createdAt")]
    pub created_at: String,
    /// Monotonic optimistic-concurrency revision.
    pub revision: u64,
}

/// A user authorization over a client node (plan 7.4).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientAccessGrant {
    /// Grant identifier.
    #[serde(rename = "clientAccessGrantId")]
    pub client_access_grant_id: String,
    /// Authorized client node.
    #[serde(rename = "clientNodeId")]
    pub client_node_id: String,
    /// Authorized user.
    #[serde(rename = "userId")]
    pub user_id: String,
    /// Granted permissions.
    pub permissions: Vec<ClientAccessPermission>,
    /// Trust mode of the grant.
    #[serde(rename = "trustMode")]
    pub trust_mode: ClientTrustMode,
    /// Lifecycle state.
    pub state: ClientAccessGrantState,
    /// User that issued the grant.
    #[serde(rename = "grantedByUserId")]
    pub granted_by_user_id: String,
    /// Origin of the grant.
    #[serde(rename = "grantSource")]
    pub grant_source: ClientAccessGrantSource,
    /// Expiry timestamp (RFC 3339), if the grant expires.
    #[serde(rename = "expiresAt")]
    pub expires_at: Option<String>,
    /// Creation timestamp (RFC 3339).
    #[serde(rename = "createdAt")]
    pub created_at: String,
    /// Monotonic optimistic-concurrency revision.
    pub revision: u64,
}

/// The occupancy lease of a client node (plan 7.5, 12).
///
/// The database must guarantee at most one active lease per client node.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientOccupancyLease {
    /// Lease identifier.
    #[serde(rename = "clientOccupancyLeaseId")]
    pub client_occupancy_lease_id: String,
    /// Occupied client node.
    #[serde(rename = "clientNodeId")]
    pub client_node_id: String,
    /// Occupying user.
    #[serde(rename = "holderUserId")]
    pub holder_user_id: String,
    /// Lifecycle state.
    pub state: OccupancyLeaseState,
    /// Monotonic fencing token; each new occupancy gets a higher token.
    #[serde(rename = "fencingToken")]
    pub fencing_token: u64,
    /// Identifier of the claim request that created the lease.
    #[serde(rename = "claimRequestId")]
    pub claim_request_id: String,
    /// Claim timestamp (RFC 3339).
    #[serde(rename = "claimedAt")]
    pub claimed_at: Option<String>,
    /// Device acknowledgement timestamp (RFC 3339).
    #[serde(rename = "acknowledgedAt")]
    pub acknowledged_at: Option<String>,
    /// Last renewal timestamp (RFC 3339).
    #[serde(rename = "lastRenewedAt")]
    pub last_renewed_at: Option<String>,
    /// Idle expiry timestamp (RFC 3339).
    #[serde(rename = "idleExpiresAt")]
    pub idle_expires_at: Option<String>,
    /// Recovery deadline after a disconnect (RFC 3339).
    #[serde(rename = "recoveryDeadlineAt")]
    pub recovery_deadline_at: Option<String>,
    /// Release request timestamp (RFC 3339).
    #[serde(rename = "releaseRequestedAt")]
    pub release_requested_at: Option<String>,
    /// Release completion timestamp (RFC 3339).
    #[serde(rename = "releasedAt")]
    pub released_at: Option<String>,
    /// Reason recorded at release.
    #[serde(rename = "releaseReason")]
    pub release_reason: Option<String>,
    /// Monotonic optimistic-concurrency revision.
    pub revision: u64,
}

/// A repository registered on a client node (plan 7.6).
///
/// The server projection never carries an absolute local path.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepositoryBinding {
    /// Binding identifier.
    #[serde(rename = "repositoryBindingId")]
    pub repository_binding_id: String,
    /// Client node hosting the repository.
    #[serde(rename = "clientNodeId")]
    pub client_node_id: String,
    /// Human-readable repository name.
    #[serde(rename = "displayName")]
    pub display_name: String,
    /// Repository kind.
    #[serde(rename = "repositoryKind")]
    pub repository_kind: RepositoryKind,
    /// Default branch name, if known.
    #[serde(rename = "defaultBranch")]
    pub default_branch: Option<String>,
    /// Current HEAD commit, if known.
    #[serde(rename = "headCommit")]
    pub head_commit: Option<String>,
    /// Dirty projection of the working tree.
    #[serde(rename = "dirtyState")]
    pub dirty_state: RepositoryDirtyState,
    /// Availability projection.
    pub availability: RepositoryAvailability,
    /// Fingerprint binding the repository identity.
    #[serde(rename = "repositoryFingerprint")]
    pub repository_fingerprint: String,
    /// Last scan timestamp (RFC 3339).
    #[serde(rename = "lastScannedAt")]
    pub last_scanned_at: Option<String>,
    /// Monotonic optimistic-concurrency revision.
    pub revision: u64,
}

/// A user authorization over a repository binding (plan 7.7).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepositoryAccessGrant {
    /// Grant identifier.
    #[serde(rename = "repositoryAccessGrantId")]
    pub repository_access_grant_id: String,
    /// Authorized repository binding.
    #[serde(rename = "repositoryBindingId")]
    pub repository_binding_id: String,
    /// Authorized user.
    #[serde(rename = "userId")]
    pub user_id: String,
    /// Granted permissions.
    pub permissions: Vec<RepositoryAccessPermission>,
    /// Lifecycle state.
    pub state: RepositoryAccessGrantState,
    /// User that issued the grant.
    #[serde(rename = "grantedByUserId")]
    pub granted_by_user_id: String,
    /// Creation timestamp (RFC 3339).
    #[serde(rename = "createdAt")]
    pub created_at: String,
    /// Monotonic optimistic-concurrency revision.
    pub revision: u64,
}

/// A single-use grant to launch one worker (plan 7.8, 14).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerLaunchGrant {
    /// Grant identifier.
    #[serde(rename = "workerLaunchGrantId")]
    pub worker_launch_grant_id: String,
    /// Client node to launch on.
    #[serde(rename = "clientNodeId")]
    pub client_node_id: String,
    /// Expected device instance identity.
    #[serde(rename = "clientInstanceId")]
    pub client_instance_id: String,
    /// Occupancy lease authorizing the launch.
    #[serde(rename = "occupancyLeaseId")]
    pub occupancy_lease_id: String,
    /// Fencing token of the occupancy lease.
    #[serde(rename = "occupancyFencingToken")]
    pub occupancy_fencing_token: u64,
    /// Repository to run against.
    #[serde(rename = "repositoryBindingId")]
    pub repository_binding_id: String,
    /// Product session the launch belongs to.
    #[serde(rename = "productSessionId")]
    pub product_session_id: String,
    /// Stage run the launch belongs to, if assigned.
    #[serde(rename = "stageRunId")]
    pub stage_run_id: Option<String>,
    /// Worker session to start.
    #[serde(rename = "workerSessionId")]
    pub worker_session_id: String,
    /// Worker identity.
    #[serde(rename = "workerId")]
    pub worker_id: String,
    /// Worker instance identity.
    #[serde(rename = "workerInstanceId")]
    pub worker_instance_id: String,
    /// Digest of the worker credential.
    #[serde(rename = "credentialDigest")]
    pub credential_digest: String,
    /// Expiry timestamp (RFC 3339).
    #[serde(rename = "expiresAt")]
    pub expires_at: String,
    /// Lifecycle state.
    pub state: WorkerLaunchGrantState,
    /// Monotonic optimistic-concurrency revision.
    pub revision: u64,
}

/// Device-local record of a retained candidate (plan 7.9, 15).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalCandidateReceipt {
    /// Receipt identifier.
    #[serde(rename = "localCandidateReceiptId")]
    pub local_candidate_receipt_id: String,
    /// Product-level candidate reference.
    #[serde(rename = "candidateRef")]
    pub candidate_ref: String,
    /// Repository binding the candidate belongs to.
    #[serde(rename = "repositoryBindingId")]
    pub repository_binding_id: String,
    /// Commit of the frozen candidate.
    #[serde(rename = "candidateCommit")]
    pub candidate_commit: String,
    /// Local Git ref name holding the candidate.
    #[serde(rename = "localRefName")]
    pub local_ref_name: String,
    /// Lifecycle state.
    pub state: LocalCandidateState,
    /// Creation timestamp (RFC 3339).
    #[serde(rename = "createdAt")]
    pub created_at: String,
    /// Monotonic optimistic-concurrency revision.
    pub revision: u64,
}

/// Device-local record of a candidate apply attempt (plan 7.10, 15.5).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalApplyReceipt {
    /// Receipt identifier.
    #[serde(rename = "localApplyReceiptId")]
    pub local_apply_receipt_id: String,
    /// Product-level candidate reference.
    #[serde(rename = "candidateRef")]
    pub candidate_ref: String,
    /// Repository binding that was applied to.
    #[serde(rename = "repositoryBindingId")]
    pub repository_binding_id: String,
    /// Target branch of the apply.
    #[serde(rename = "targetBranch")]
    pub target_branch: String,
    /// Expected target HEAD before the apply, if pinned.
    #[serde(rename = "expectedHead")]
    pub expected_head: Option<String>,
    /// Apply strategy.
    pub strategy: ApplyStrategy,
    /// Terminal result.
    pub result: ApplyResult,
    /// Commit produced by the apply, if any.
    #[serde(rename = "resultingCommit")]
    pub resulting_commit: Option<String>,
    /// Reference to a conflict artifact, if conflicts occurred.
    #[serde(rename = "conflictArtifactRef")]
    pub conflict_artifact_ref: Option<String>,
    /// Creation timestamp (RFC 3339).
    #[serde(rename = "createdAt")]
    pub created_at: String,
    /// Monotonic optimistic-concurrency revision.
    pub revision: u64,
}

#[cfg(test)]
mod tests {
    use serde::de::DeserializeOwned;

    use super::*;

    /// Deserializes `json`, re-serializes it, and asserts both the struct
    /// round-trip and the semantic JSON equality.
    fn assert_round_trip<T>(json: &str)
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
    }

    #[test]
    fn user_account_round_trips() {
        assert_round_trip::<UserAccount>(
            r#"{
                "userId": "usr_01j2",
                "username": "Alice",
                "normalizedUsername": "alice",
                "passwordHash": "argon2id$abc123",
                "role": "owner",
                "state": "active",
                "createdAt": "2026-01-01T00:00:00Z",
                "updatedAt": "2026-01-02T00:00:00Z",
                "revision": 3
            }"#,
        );
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
    fn client_occupancy_lease_round_trips() {
        assert_round_trip::<ClientOccupancyLease>(
            r#"{
                "clientOccupancyLeaseId": "lease_01j2",
                "clientNodeId": "node_01j2",
                "holderUserId": "usr_01j2",
                "state": "occupied",
                "fencingToken": 7,
                "claimRequestId": "claim_01j2",
                "claimedAt": "2026-01-02T12:00:00Z",
                "acknowledgedAt": "2026-01-02T12:00:01Z",
                "lastRenewedAt": "2026-01-02T12:05:00Z",
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
                "occupancyFencingToken": 7,
                "repositoryBindingId": "rb_01j2",
                "productSessionId": "ps_01j2",
                "stageRunId": null,
                "workerSessionId": "ws_01j2",
                "workerId": "worker_1",
                "workerInstanceId": "winst_01j2",
                "credentialDigest": "sha256:dd44",
                "expiresAt": "2026-01-02T12:10:00Z",
                "state": "issued",
                "revision": 1
            }"#,
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
