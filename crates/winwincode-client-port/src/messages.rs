// SPDX-License-Identifier: Apache-2.0

//! `ClientControlPort` envelope and message contract (plan section 9).
//!
//! Every message travels in an [`Envelope`] carrying the frame fields plus a
//! `kind`/`payload` pair. All payloads embed the command context fields
//! (`expectedRevision`, `idempotencyKey`); occupancy- and execution-related
//! payloads additionally embed `occupancyLeaseId` and `occupancyFencingToken`
//! (plan section 9.5, 12.6).
//!
//! The `kind` strings match the plan enumeration exactly, for example
//! `client.enroll` and `client.enrollment_accepted`.

#![allow(clippy::large_enum_variant)]

use serde::Deserialize;
use serde::Serialize;

use crate::domain::ApplyStrategy;
use crate::domain::ClientAccessPermission;
use crate::domain::ClientConnectCode;
use crate::domain::ClientLockState;
use crate::domain::ClientOccupancyLease;
use crate::domain::LocalApplyReceipt;
use crate::domain::LocalCandidateReceipt;
use crate::domain::PresenceState;
use crate::domain::RepositoryAvailability;
use crate::domain::RepositoryDirtyState;
use crate::domain::RepositoryKind;
use crate::domain::WorkerLaunchGrant;

/// Current `schemaVersion` of the `ClientControlPort` wire contract.
pub const CLIENT_CONTROL_PORT_SCHEMA_VERSION: u32 = 1;

/// Common command fields every `ClientControlPort` payload carries
/// (plan section 9.5).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandContext {
    /// Repository/object revision the sender derived the command from.
    #[serde(rename = "expectedRevision")]
    pub expected_revision: u64,
    /// Idempotency key making the command safe to replay.
    #[serde(rename = "idempotencyKey")]
    pub idempotency_key: String,
}

/// Command context extended with the occupancy fencing stamp required from
/// occupancy- and execution-related commands (plan section 9.5, 12.6).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct OccupancyCommandContext {
    /// Common command fields.
    #[serde(flatten)]
    pub command: CommandContext,
    /// Occupancy lease the command is bound to.
    #[serde(rename = "occupancyLeaseId")]
    pub occupancy_lease_id: String,
    /// Fencing token of the occupancy lease; stale tokens are rejected.
    #[serde(rename = "occupancyFencingToken")]
    pub occupancy_fencing_token: u64,
}

/// Envelope frame for `ClientControlPort` messages (plan section 9.5).
///
/// The `kind` and `payload` fields come from the flattened [`ClientToServerMessage`]
/// or [`ServerToClientMessage`] enum, so a serialized envelope has exactly the
/// plan's `schemaVersion`/`messageId`/`clientNodeId`/`clientInstanceId`/
/// `sequence`/`occurredAt`/`kind`/`payload` shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Envelope<M> {
    /// Wire contract schema version.
    #[serde(rename = "schemaVersion")]
    pub schema_version: u32,
    /// Unique message identifier.
    #[serde(rename = "messageId")]
    pub message_id: String,
    /// Sending client node.
    #[serde(rename = "clientNodeId")]
    pub client_node_id: String,
    /// Device process instance that produced the message.
    #[serde(rename = "clientInstanceId")]
    pub client_instance_id: String,
    /// Per-sender monotonic sequence number for replay protection.
    pub sequence: u64,
    /// Occurrence timestamp (RFC 3339).
    #[serde(rename = "occurredAt")]
    pub occurred_at: String,
    /// Typed message: the `kind`/`payload` pair.
    #[serde(flatten)]
    pub message: M,
}

/// Envelope carrying client-to-server messages.
pub type ClientToServerEnvelope = Envelope<ClientToServerMessage>;

/// Envelope carrying server-to-client messages.
pub type ServerToClientEnvelope = Envelope<ServerToClientMessage>;

/// Payload of `client.enroll` (plan section 9.3).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ClientEnrollPayload {
    /// Common command fields.
    #[serde(flatten)]
    pub command: CommandContext,
    /// Requested device display name.
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
    pub device_credential_digest: String,
}

/// Payload of `client.hello` (plan section 9.3).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ClientHelloPayload {
    /// Common command fields.
    #[serde(flatten)]
    pub command: CommandContext,
    /// Device client software version.
    #[serde(rename = "clientVersion")]
    pub client_version: String,
    /// Whether the node currently accepts new connections.
    #[serde(rename = "acceptingConnections")]
    pub accepting_connections: bool,
    /// Maximum concurrent worker sessions.
    #[serde(rename = "maxConcurrentWorkerSessions")]
    pub max_concurrent_worker_sessions: u32,
}

/// Payload of `client.heartbeat` (plan section 9.3).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ClientHeartbeatPayload {
    /// Common command fields.
    #[serde(flatten)]
    pub command: CommandContext,
    /// Worker sessions currently running on the device.
    #[serde(rename = "reportedRunningWorkerSessions")]
    pub reported_running_worker_sessions: u32,
    /// Whether the node currently accepts new connections.
    #[serde(rename = "acceptingConnections")]
    pub accepting_connections: bool,
}

/// Payload of `client.connect_code.published` (plan section 9.3, 11.3).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ClientConnectCodePublishedPayload {
    /// Common command fields.
    #[serde(flatten)]
    pub command: CommandContext,
    /// Published connect code projection.
    #[serde(rename = "connectCode")]
    pub connect_code: ClientConnectCode,
}

/// Payload of `client.access.challenge_ack` (plan section 9.3, 11.4).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ClientAccessChallengeAckPayload {
    /// Common command fields.
    #[serde(flatten)]
    pub command: CommandContext,
    /// Challenge being acknowledged.
    #[serde(rename = "challengeId")]
    pub challenge_id: String,
    /// Whether the device confirmed the challenge.
    pub confirmed: bool,
    /// Rejection reason, if not confirmed.
    pub reason: Option<String>,
    /// Permissions requested for the connecting user.
    #[serde(rename = "requestedPermissions")]
    pub requested_permissions: Vec<ClientAccessPermission>,
}

/// Payload of `client.occupancy.ack` (plan section 9.3, 12.2).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ClientOccupancyAckPayload {
    /// Command fields with the occupancy fencing stamp.
    #[serde(flatten)]
    pub occupancy: OccupancyCommandContext,
    /// Acknowledgement timestamp (RFC 3339).
    #[serde(rename = "acknowledgedAt")]
    pub acknowledged_at: String,
}

/// Payload of `client.occupancy.rejected` (plan section 9.3, 12.6).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ClientOccupancyRejectedPayload {
    /// Command fields with the occupancy fencing stamp.
    #[serde(flatten)]
    pub occupancy: OccupancyCommandContext,
    /// Why the device rejected the occupancy.
    pub reason: String,
}

/// Payload of `client.repository.upsert` (plan section 9.3, 13.1).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ClientRepositoryUpsertPayload {
    /// Common command fields.
    #[serde(flatten)]
    pub command: CommandContext,
    /// Binding the report refers to.
    #[serde(rename = "repositoryBindingId")]
    pub repository_binding_id: String,
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
}

/// Payload of `client.repository.removed` (plan section 9.3).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ClientRepositoryRemovedPayload {
    /// Common command fields.
    #[serde(flatten)]
    pub command: CommandContext,
    /// Binding that was removed locally.
    #[serde(rename = "repositoryBindingId")]
    pub repository_binding_id: String,
}

/// Payload of `client.repository.status` (plan section 9.3, 13.5).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ClientRepositoryStatusPayload {
    /// Common command fields.
    #[serde(flatten)]
    pub command: CommandContext,
    /// Binding the status refers to.
    #[serde(rename = "repositoryBindingId")]
    pub repository_binding_id: String,
    /// Availability projection.
    pub availability: RepositoryAvailability,
    /// Current HEAD commit, if known.
    #[serde(rename = "headCommit")]
    pub head_commit: Option<String>,
    /// Dirty projection of the working tree.
    #[serde(rename = "dirtyState")]
    pub dirty_state: RepositoryDirtyState,
    /// Last scan timestamp (RFC 3339).
    #[serde(rename = "lastScannedAt")]
    pub last_scanned_at: Option<String>,
}

/// Payload of `client.worker.launch_ack` (plan section 9.3, 14.3).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ClientWorkerLaunchAckPayload {
    /// Command fields with the occupancy fencing stamp.
    #[serde(flatten)]
    pub occupancy: OccupancyCommandContext,
    /// Grant that was consumed.
    #[serde(rename = "workerLaunchGrantId")]
    pub worker_launch_grant_id: String,
    /// Worker session that was started.
    #[serde(rename = "workerSessionId")]
    pub worker_session_id: String,
    /// Worker identity.
    #[serde(rename = "workerId")]
    pub worker_id: String,
    /// Worker instance identity.
    #[serde(rename = "workerInstanceId")]
    pub worker_instance_id: String,
    /// Whether the launch succeeded.
    pub accepted: bool,
    /// Failure reason, if not accepted.
    pub reason: Option<String>,
}

/// Payload of `client.worker.state` (plan section 9.3).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ClientWorkerStatePayload {
    /// Command fields with the occupancy fencing stamp.
    #[serde(flatten)]
    pub occupancy: OccupancyCommandContext,
    /// Worker session the state refers to.
    #[serde(rename = "workerSessionId")]
    pub worker_session_id: String,
    /// Worker identity.
    #[serde(rename = "workerId")]
    pub worker_id: String,
    /// Worker instance identity.
    #[serde(rename = "workerInstanceId")]
    pub worker_instance_id: String,
    /// Device-observed process state; the value set is owned by the
    /// existing worker contract.
    pub state: String,
    /// Stage run the worker is executing, if any.
    #[serde(rename = "stageRunId")]
    pub stage_run_id: Option<String>,
    /// Observation timestamp (RFC 3339).
    #[serde(rename = "observedAt")]
    pub observed_at: String,
}

/// One reconciled worker process in `client.worker.reconcile`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientWorkerReconcileEntry {
    /// Worker session the entry refers to.
    #[serde(rename = "workerSessionId")]
    pub worker_session_id: String,
    /// Worker identity.
    #[serde(rename = "workerId")]
    pub worker_id: String,
    /// Worker instance identity.
    #[serde(rename = "workerInstanceId")]
    pub worker_instance_id: String,
    /// Device-observed process state; the value set is owned by the
    /// existing worker contract.
    pub state: String,
}

/// Payload of `client.worker.reconcile` (plan section 9.3, 12.5).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ClientWorkerReconcilePayload {
    /// Command fields with the occupancy fencing stamp.
    #[serde(flatten)]
    pub occupancy: OccupancyCommandContext,
    /// Locally observed worker processes after reconnection.
    pub workers: Vec<ClientWorkerReconcileEntry>,
}

/// Payload of `client.candidate.retained` (plan section 9.3, 15.2).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ClientCandidateRetainedPayload {
    /// Common command fields.
    #[serde(flatten)]
    pub command: CommandContext,
    /// Device-local retention receipt.
    #[serde(rename = "receipt")]
    pub receipt: LocalCandidateReceipt,
}

/// Payload of `client.candidate.apply_result` (plan section 9.3, 15.5).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ClientCandidateApplyResultPayload {
    /// Command fields with the occupancy fencing stamp.
    #[serde(flatten)]
    pub occupancy: OccupancyCommandContext,
    /// Device-local apply receipt.
    #[serde(rename = "receipt")]
    pub receipt: LocalApplyReceipt,
}

/// Payload of `client.command_ack` (plan section 9.3).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ClientCommandAckPayload {
    /// Common command fields.
    #[serde(flatten)]
    pub command: CommandContext,
    /// Message id of the server message being acknowledged.
    #[serde(rename = "acknowledgedMessageId")]
    pub acknowledged_message_id: String,
    /// Kind of the server message being acknowledged.
    #[serde(rename = "acknowledgedKind")]
    pub acknowledged_kind: Option<String>,
    /// Whether the device accepted the server command.
    pub accepted: bool,
    /// Failure reason, if not accepted.
    pub reason: Option<String>,
}

/// A client-to-server `ClientControlPort` message (plan section 9.3).
///
/// Serialized as a `kind`/`payload` pair using the plan's exact kind strings.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "payload")]
pub enum ClientToServerMessage {
    /// Enroll a fresh device with the server.
    #[serde(rename = "client.enroll")]
    Enroll(Box<ClientEnrollPayload>),
    /// Announce a device instance after (re)connect.
    #[serde(rename = "client.hello")]
    Hello(ClientHelloPayload),
    /// Periodic presence and capacity report.
    #[serde(rename = "client.heartbeat")]
    Heartbeat(ClientHeartbeatPayload),
    /// A dynamic connect code was published on the device.
    #[serde(rename = "client.connect_code.published")]
    ConnectCodePublished(ClientConnectCodePublishedPayload),
    /// Device response to an access challenge.
    #[serde(rename = "client.access.challenge_ack")]
    AccessChallengeAck(Box<ClientAccessChallengeAckPayload>),
    /// Device acknowledged an occupancy offer under its fencing token.
    #[serde(rename = "client.occupancy.ack")]
    OccupancyAck(ClientOccupancyAckPayload),
    /// Device rejected an occupancy under its fencing token.
    #[serde(rename = "client.occupancy.rejected")]
    OccupancyRejected(ClientOccupancyRejectedPayload),
    /// A repository binding was created or updated locally.
    #[serde(rename = "client.repository.upsert")]
    RepositoryUpsert(ClientRepositoryUpsertPayload),
    /// A repository binding was removed locally.
    #[serde(rename = "client.repository.removed")]
    RepositoryRemoved(ClientRepositoryRemovedPayload),
    /// A repository scan produced a new status projection.
    #[serde(rename = "client.repository.status")]
    RepositoryStatus(ClientRepositoryStatusPayload),
    /// Device consumed a worker launch grant.
    #[serde(rename = "client.worker.launch_ack")]
    WorkerLaunchAck(Box<ClientWorkerLaunchAckPayload>),
    /// Device observed a worker process state change.
    #[serde(rename = "client.worker.state")]
    WorkerState(ClientWorkerStatePayload),
    /// Device reconciled local workers after reconnection.
    #[serde(rename = "client.worker.reconcile")]
    WorkerReconcile(ClientWorkerReconcilePayload),
    /// Device retained a candidate locally.
    #[serde(rename = "client.candidate.retained")]
    CandidateRetained(ClientCandidateRetainedPayload),
    /// Device reports the result of a candidate apply.
    #[serde(rename = "client.candidate.apply_result")]
    CandidateApplyResult(ClientCandidateApplyResultPayload),
    /// Device acknowledges a server-to-client command.
    #[serde(rename = "client.command_ack")]
    CommandAck(ClientCommandAckPayload),
}

/// Payload of `client.enrollment_accepted` (plan section 9.4, 11.4).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ServerEnrollmentAcceptedPayload {
    /// Common command fields.
    #[serde(flatten)]
    pub command: CommandContext,
    /// Stable public device identifier, not a secret.
    #[serde(rename = "publicClientId")]
    pub public_client_id: String,
    /// Presence state assigned after enrollment.
    #[serde(rename = "presenceState")]
    pub presence_state: PresenceState,
}

/// Payload of `client.access.challenge` (plan section 9.4, 11.4).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ServerAccessChallengePayload {
    /// Common command fields.
    #[serde(flatten)]
    pub command: CommandContext,
    /// Challenge identifier.
    #[serde(rename = "challengeId")]
    pub challenge_id: String,
    /// Connect code being verified, if the flow uses one.
    #[serde(rename = "connectCodeId")]
    pub connect_code_id: Option<String>,
    /// Expiry timestamp (RFC 3339).
    #[serde(rename = "expiresAt")]
    pub expires_at: String,
    /// Remaining verification attempts.
    #[serde(rename = "remainingAttempts")]
    pub remaining_attempts: u32,
}

/// Payload of `client.occupancy.offer` (plan section 9.4, 12.2).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ServerOccupancyOfferPayload {
    /// Common command fields.
    #[serde(flatten)]
    pub command: CommandContext,
    /// Created occupancy lease awaiting device acknowledgement.
    #[serde(rename = "lease")]
    pub lease: ClientOccupancyLease,
}

/// Payload of `client.occupancy.release` (plan section 9.4, 12.4).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ServerOccupancyReleasePayload {
    /// Command fields with the occupancy fencing stamp.
    #[serde(flatten)]
    pub occupancy: OccupancyCommandContext,
    /// Lease being released.
    #[serde(rename = "clientOccupancyLeaseId")]
    pub client_occupancy_lease_id: String,
    /// Whether the device must drain active tasks before releasing.
    pub drain: bool,
    /// Release reason.
    pub reason: Option<String>,
}

/// Payload of `client.occupancy.force_fence` (plan section 9.4, 12.6).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ServerOccupancyForceFencePayload {
    /// Command fields with the occupancy fencing stamp.
    #[serde(flatten)]
    pub occupancy: OccupancyCommandContext,
    /// Lease being re-fenced.
    #[serde(rename = "clientOccupancyLeaseId")]
    pub client_occupancy_lease_id: String,
    /// New fencing token that supersedes all earlier tokens.
    #[serde(rename = "newFencingToken")]
    pub new_fencing_token: u64,
}

/// Payload of `client.repository.rescan` (plan section 9.4, 13.5).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ServerRepositoryRescanPayload {
    /// Common command fields.
    #[serde(flatten)]
    pub command: CommandContext,
    /// Binding that must be rescanned.
    #[serde(rename = "repositoryBindingId")]
    pub repository_binding_id: String,
}

/// Payload of `client.worker.launch` (plan section 9.4, 14.3).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ServerWorkerLaunchPayload {
    /// Command fields with the occupancy fencing stamp.
    #[serde(flatten)]
    pub occupancy: OccupancyCommandContext,
    /// Single-use launch grant to consume.
    #[serde(rename = "grant")]
    pub grant: WorkerLaunchGrant,
}

/// Payload of `client.worker.stop` (plan section 9.4, 12.6).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ServerWorkerStopPayload {
    /// Command fields with the occupancy fencing stamp.
    #[serde(flatten)]
    pub occupancy: OccupancyCommandContext,
    /// Worker session that must stop.
    #[serde(rename = "workerSessionId")]
    pub worker_session_id: String,
    /// Stop reason.
    pub reason: Option<String>,
}

/// Payload of `client.candidate.apply` (plan section 9.4, 15.4).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ServerCandidateApplyPayload {
    /// Command fields with the occupancy fencing stamp.
    #[serde(flatten)]
    pub occupancy: OccupancyCommandContext,
    /// Product-level candidate reference to apply.
    #[serde(rename = "candidateRef")]
    pub candidate_ref: String,
    /// Binding to apply the candidate to.
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
}

/// Payload of `client.client_lock` (plan section 9.4, 12.1).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ServerClientLockPayload {
    /// Common command fields.
    #[serde(flatten)]
    pub command: CommandContext,
    /// New machine-level lock state.
    #[serde(rename = "lockState")]
    pub lock_state: ClientLockState,
    /// Reason for the lock change.
    pub reason: Option<String>,
}

/// Payload of `client.credential_rotate` (plan section 9.4).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ServerCredentialRotatePayload {
    /// Common command fields.
    #[serde(flatten)]
    pub command: CommandContext,
    /// Deadline by which the device must rotate (RFC 3339), if bounded.
    #[serde(rename = "rotateBy")]
    pub rotate_by: Option<String>,
    /// Reason for the rotation.
    pub reason: Option<String>,
}

/// A server-to-client `ClientControlPort` message (plan section 9.4).
///
/// Serialized as a `kind`/`payload` pair using the plan's exact kind strings.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "payload")]
pub enum ServerToClientMessage {
    /// Device enrollment was accepted.
    #[serde(rename = "client.enrollment_accepted")]
    EnrollmentAccepted(ServerEnrollmentAcceptedPayload),
    /// Server challenges a connecting user.
    #[serde(rename = "client.access.challenge")]
    AccessChallenge(Box<ServerAccessChallengePayload>),
    /// Server offers a created occupancy lease.
    #[serde(rename = "client.occupancy.offer")]
    OccupancyOffer(ServerOccupancyOfferPayload),
    /// Server requests an occupancy release.
    #[serde(rename = "client.occupancy.release")]
    OccupancyRelease(ServerOccupancyReleasePayload),
    /// Server re-fences an occupancy with a higher token.
    #[serde(rename = "client.occupancy.force_fence")]
    OccupancyForceFence(ServerOccupancyForceFencePayload),
    /// Server requests a repository rescan.
    #[serde(rename = "client.repository.rescan")]
    RepositoryRescan(ServerRepositoryRescanPayload),
    /// Server delivers a worker launch grant.
    #[serde(rename = "client.worker.launch")]
    WorkerLaunch(ServerWorkerLaunchPayload),
    /// Server requests a worker stop.
    #[serde(rename = "client.worker.stop")]
    WorkerStop(ServerWorkerStopPayload),
    /// Server requests a candidate apply.
    #[serde(rename = "client.candidate.apply")]
    CandidateApply(ServerCandidateApplyPayload),
    /// Server changes the machine-level client lock.
    #[serde(rename = "client.client_lock")]
    ClientLock(ServerClientLockPayload),
    /// Server requests a device credential rotation.
    #[serde(rename = "client.credential_rotate")]
    CredentialRotate(ServerCredentialRotatePayload),
}

#[cfg(test)]
mod tests {
    use serde::de::DeserializeOwned;

    use super::*;
    use crate::domain::ApplyResult;
    use crate::domain::ApplyStrategy;
    use crate::domain::ConnectCodeState;
    use crate::domain::LocalCandidateState;
    use crate::domain::OccupancyLeaseState;
    use crate::domain::WorkerLaunchGrantState;

    /// Deserializes `json`, re-serializes it, and asserts both the struct
    /// round-trip and the semantic JSON equality. Returns the reparsed value.
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
        reparsed
    }

    fn command_context() -> CommandContext {
        CommandContext {
            expected_revision: 41,
            idempotency_key: "idem_01j2".to_owned(),
        }
    }

    fn occupancy_context() -> OccupancyCommandContext {
        OccupancyCommandContext {
            command: command_context(),
            occupancy_lease_id: "lease_01j2".to_owned(),
            occupancy_fencing_token: 7,
        }
    }

    #[test]
    fn occupancy_ack_envelope_round_trips_with_fencing_fields() {
        let json = r#"{
            "schemaVersion": 1,
            "messageId": "msg_c2s_01",
            "clientNodeId": "node_01j2",
            "clientInstanceId": "inst_01j2",
            "sequence": 1024,
            "occurredAt": "2026-01-02T12:00:01Z",
            "kind": "client.occupancy.ack",
            "payload": {
                "expectedRevision": 41,
                "idempotencyKey": "idem_01j2",
                "occupancyLeaseId": "lease_01j2",
                "occupancyFencingToken": 7,
                "acknowledgedAt": "2026-01-02T12:00:01Z"
            }
        }"#;
        let envelope = assert_round_trip::<ClientToServerEnvelope>(json);
        let ClientToServerMessage::OccupancyAck(payload) = &envelope.message else {
            panic!("expected client.occupancy.ack");
        };
        // The command and fencing fields are required at the payload's top
        // level, next to the payload-specific fields (plan section 9.5).
        assert_eq!(payload.occupancy.occupancy_fencing_token, 7);
        assert_eq!(payload.occupancy.occupancy_lease_id, "lease_01j2");
        assert_eq!(payload.occupancy.command.expected_revision, 41);
        assert_eq!(payload.occupancy.command.idempotency_key, "idem_01j2");
    }

    #[test]
    fn worker_launch_envelope_round_trips() {
        let json = r#"{
            "schemaVersion": 1,
            "messageId": "msg_s2c_01",
            "clientNodeId": "node_01j2",
            "clientInstanceId": "inst_01j2",
            "sequence": 512,
            "occurredAt": "2026-01-02T12:05:00Z",
            "kind": "client.worker.launch",
            "payload": {
                "expectedRevision": 41,
                "idempotencyKey": "idem_s2c_01",
                "occupancyLeaseId": "lease_01j2",
                "occupancyFencingToken": 7,
                "grant": {
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
                }
            }
        }"#;
        let envelope = assert_round_trip::<ServerToClientEnvelope>(json);
        let ServerToClientMessage::WorkerLaunch(payload) = &envelope.message else {
            panic!("expected client.worker.launch");
        };
        assert_eq!(payload.grant.worker_session_id, "ws_01j2");
        assert_eq!(payload.occupancy.occupancy_fencing_token, 7);
        assert_eq!(payload.grant.occupancy_fencing_token, 7);
    }

    #[test]
    fn enroll_envelope_round_trips_with_boxed_payload() {
        let json = r#"{
            "schemaVersion": 1,
            "messageId": "msg_c2s_02",
            "clientNodeId": "node_01j2",
            "clientInstanceId": "inst_01j2",
            "sequence": 1,
            "occurredAt": "2026-01-01T00:00:00Z",
            "kind": "client.enroll",
            "payload": {
                "expectedRevision": 0,
                "idempotencyKey": "idem_enroll_01",
                "displayName": "Cheng's MacBook",
                "platform": "darwin",
                "architecture": "arm64",
                "clientVersion": "0.1.0-alpha.1",
                "deviceCredentialDigest": "sha256:aa11"
            }
        }"#;
        let envelope = assert_round_trip::<ClientToServerEnvelope>(json);
        let ClientToServerMessage::Enroll(payload) = &envelope.message else {
            panic!("expected client.enroll");
        };
        assert_eq!(payload.display_name, "Cheng's MacBook");
        assert_eq!(payload.architecture, "arm64");
    }

    /// Builds one representative message of every client-to-server kind.
    #[allow(clippy::too_many_lines)]
    fn all_client_to_server_messages() -> Vec<(&'static str, ClientToServerMessage)> {
        let command = command_context();
        let occupancy = occupancy_context();
        vec![
            (
                "client.enroll",
                ClientToServerMessage::Enroll(Box::new(ClientEnrollPayload {
                    command: command.clone(),
                    display_name: "Cheng's MacBook".to_owned(),
                    platform: "darwin".to_owned(),
                    architecture: "arm64".to_owned(),
                    client_version: "0.1.0-alpha.1".to_owned(),
                    device_credential_digest: "sha256:aa11".to_owned(),
                })),
            ),
            (
                "client.hello",
                ClientToServerMessage::Hello(ClientHelloPayload {
                    command: command.clone(),
                    client_version: "0.1.0-alpha.1".to_owned(),
                    accepting_connections: true,
                    max_concurrent_worker_sessions: 3,
                }),
            ),
            (
                "client.heartbeat",
                ClientToServerMessage::Heartbeat(ClientHeartbeatPayload {
                    command: command.clone(),
                    reported_running_worker_sessions: 1,
                    accepting_connections: true,
                }),
            ),
            (
                "client.connect_code.published",
                ClientToServerMessage::ConnectCodePublished(ClientConnectCodePublishedPayload {
                    command: command.clone(),
                    connect_code: ClientConnectCode {
                        connect_code_id: "code_01j2".to_owned(),
                        client_node_id: "node_01j2".to_owned(),
                        code_digest: "sha256:bb22".to_owned(),
                        issued_by_instance_id: "inst_01j2".to_owned(),
                        expires_at: "2026-01-01T01:00:00Z".to_owned(),
                        remaining_attempts: 5,
                        state: ConnectCodeState::Active,
                        created_at: "2026-01-01T00:00:00Z".to_owned(),
                        revision: 1,
                    },
                }),
            ),
            (
                "client.access.challenge_ack",
                ClientToServerMessage::AccessChallengeAck(Box::new(
                    ClientAccessChallengeAckPayload {
                        command: command.clone(),
                        challenge_id: "chal_01j2".to_owned(),
                        confirmed: true,
                        reason: None,
                        requested_permissions: vec![ClientAccessPermission::Use],
                    },
                )),
            ),
            (
                "client.occupancy.ack",
                ClientToServerMessage::OccupancyAck(ClientOccupancyAckPayload {
                    occupancy: occupancy.clone(),
                    acknowledged_at: "2026-01-02T12:00:01Z".to_owned(),
                }),
            ),
            (
                "client.occupancy.rejected",
                ClientToServerMessage::OccupancyRejected(ClientOccupancyRejectedPayload {
                    occupancy: occupancy.clone(),
                    reason: "stale fencing token".to_owned(),
                }),
            ),
            (
                "client.repository.upsert",
                ClientToServerMessage::RepositoryUpsert(ClientRepositoryUpsertPayload {
                    command: command.clone(),
                    repository_binding_id: "rb_01j2".to_owned(),
                    display_name: "winwincode".to_owned(),
                    repository_kind: RepositoryKind::Git,
                    default_branch: Some("main".to_owned()),
                    head_commit: Some("0123456789abcdef0123456789abcdef01234567".to_owned()),
                    dirty_state: RepositoryDirtyState::Clean,
                    availability: RepositoryAvailability::Available,
                    repository_fingerprint: "sha256:cc33".to_owned(),
                    last_scanned_at: Some("2026-01-02T11:00:00Z".to_owned()),
                }),
            ),
            (
                "client.repository.removed",
                ClientToServerMessage::RepositoryRemoved(ClientRepositoryRemovedPayload {
                    command: command.clone(),
                    repository_binding_id: "rb_01j2".to_owned(),
                }),
            ),
            (
                "client.repository.status",
                ClientToServerMessage::RepositoryStatus(ClientRepositoryStatusPayload {
                    command: command.clone(),
                    repository_binding_id: "rb_01j2".to_owned(),
                    availability: RepositoryAvailability::Available,
                    head_commit: Some("0123456789abcdef0123456789abcdef01234567".to_owned()),
                    dirty_state: RepositoryDirtyState::Clean,
                    last_scanned_at: Some("2026-01-02T11:00:00Z".to_owned()),
                }),
            ),
            (
                "client.worker.launch_ack",
                ClientToServerMessage::WorkerLaunchAck(Box::new(ClientWorkerLaunchAckPayload {
                    occupancy: occupancy.clone(),
                    worker_launch_grant_id: "wlg_01j2".to_owned(),
                    worker_session_id: "ws_01j2".to_owned(),
                    worker_id: "worker_1".to_owned(),
                    worker_instance_id: "winst_01j2".to_owned(),
                    accepted: true,
                    reason: None,
                })),
            ),
            (
                "client.worker.state",
                ClientToServerMessage::WorkerState(ClientWorkerStatePayload {
                    occupancy: occupancy.clone(),
                    worker_session_id: "ws_01j2".to_owned(),
                    worker_id: "worker_1".to_owned(),
                    worker_instance_id: "winst_01j2".to_owned(),
                    state: "running".to_owned(),
                    stage_run_id: Some("stage_01j2".to_owned()),
                    observed_at: "2026-01-02T12:06:00Z".to_owned(),
                }),
            ),
            (
                "client.worker.reconcile",
                ClientToServerMessage::WorkerReconcile(ClientWorkerReconcilePayload {
                    occupancy: occupancy.clone(),
                    workers: vec![ClientWorkerReconcileEntry {
                        worker_session_id: "ws_01j2".to_owned(),
                        worker_id: "worker_1".to_owned(),
                        worker_instance_id: "winst_01j2".to_owned(),
                        state: "running".to_owned(),
                    }],
                }),
            ),
            (
                "client.candidate.retained",
                ClientToServerMessage::CandidateRetained(ClientCandidateRetainedPayload {
                    command: command.clone(),
                    receipt: LocalCandidateReceipt {
                        local_candidate_receipt_id: "lcr_01j2".to_owned(),
                        candidate_ref: "cand_01j2".to_owned(),
                        repository_binding_id: "rb_01j2".to_owned(),
                        candidate_commit: "89abcdef0123456789abcdef0123456789abcdef".to_owned(),
                        local_ref_name: "refs/winwincode/candidates/cand_01j2".to_owned(),
                        state: LocalCandidateState::Retained,
                        created_at: "2026-01-02T12:30:00Z".to_owned(),
                        revision: 1,
                    },
                }),
            ),
            (
                "client.candidate.apply_result",
                ClientToServerMessage::CandidateApplyResult(ClientCandidateApplyResultPayload {
                    occupancy: occupancy.clone(),
                    receipt: LocalApplyReceipt {
                        local_apply_receipt_id: "lar_01j2".to_owned(),
                        candidate_ref: "cand_01j2".to_owned(),
                        repository_binding_id: "rb_01j2".to_owned(),
                        target_branch: "feature/x".to_owned(),
                        expected_head: Some("0123456789abcdef0123456789abcdef01234567".to_owned()),
                        strategy: ApplyStrategy::CherryPick,
                        result: ApplyResult::Applied,
                        resulting_commit: Some(
                            "fedcba9876543210fedcba9876543210fedcba98".to_owned(),
                        ),
                        conflict_artifact_ref: None,
                        created_at: "2026-01-02T12:31:00Z".to_owned(),
                        revision: 2,
                    },
                }),
            ),
            (
                "client.command_ack",
                ClientToServerMessage::CommandAck(ClientCommandAckPayload {
                    command,
                    acknowledged_message_id: "msg_s2c_01".to_owned(),
                    acknowledged_kind: Some("client.worker.launch".to_owned()),
                    accepted: true,
                    reason: None,
                }),
            ),
        ]
    }

    /// Builds one representative message of every server-to-client kind.
    #[allow(clippy::too_many_lines)]
    fn all_server_to_client_messages() -> Vec<(&'static str, ServerToClientMessage)> {
        let command = command_context();
        let occupancy = occupancy_context();
        vec![
            (
                "client.enrollment_accepted",
                ServerToClientMessage::EnrollmentAccepted(ServerEnrollmentAcceptedPayload {
                    command: command.clone(),
                    public_client_id: "pub_dev_99".to_owned(),
                    presence_state: PresenceState::Online,
                }),
            ),
            (
                "client.access.challenge",
                ServerToClientMessage::AccessChallenge(Box::new(ServerAccessChallengePayload {
                    command: command.clone(),
                    challenge_id: "chal_01j2".to_owned(),
                    connect_code_id: Some("code_01j2".to_owned()),
                    expires_at: "2026-01-01T01:00:00Z".to_owned(),
                    remaining_attempts: 5,
                })),
            ),
            (
                "client.occupancy.offer",
                ServerToClientMessage::OccupancyOffer(ServerOccupancyOfferPayload {
                    command: command.clone(),
                    lease: ClientOccupancyLease {
                        client_occupancy_lease_id: "lease_01j2".to_owned(),
                        client_node_id: "node_01j2".to_owned(),
                        holder_user_id: "usr_01j2".to_owned(),
                        state: OccupancyLeaseState::Reserving,
                        fencing_token: 7,
                        claim_request_id: "claim_01j2".to_owned(),
                        claimed_at: Some("2026-01-02T12:00:00Z".to_owned()),
                        acknowledged_at: None,
                        last_renewed_at: None,
                        idle_expires_at: None,
                        recovery_deadline_at: None,
                        release_requested_at: None,
                        released_at: None,
                        release_reason: None,
                        revision: 1,
                    },
                }),
            ),
            (
                "client.occupancy.release",
                ServerToClientMessage::OccupancyRelease(ServerOccupancyReleasePayload {
                    occupancy: occupancy.clone(),
                    client_occupancy_lease_id: "lease_01j2".to_owned(),
                    drain: true,
                    reason: None,
                }),
            ),
            (
                "client.occupancy.force_fence",
                ServerToClientMessage::OccupancyForceFence(ServerOccupancyForceFencePayload {
                    occupancy: occupancy.clone(),
                    client_occupancy_lease_id: "lease_01j2".to_owned(),
                    new_fencing_token: 9,
                }),
            ),
            (
                "client.repository.rescan",
                ServerToClientMessage::RepositoryRescan(ServerRepositoryRescanPayload {
                    command: command.clone(),
                    repository_binding_id: "rb_01j2".to_owned(),
                }),
            ),
            (
                "client.worker.launch",
                ServerToClientMessage::WorkerLaunch(ServerWorkerLaunchPayload {
                    occupancy: occupancy.clone(),
                    grant: WorkerLaunchGrant {
                        worker_launch_grant_id: "wlg_01j2".to_owned(),
                        client_node_id: "node_01j2".to_owned(),
                        client_instance_id: "inst_01j2".to_owned(),
                        occupancy_lease_id: "lease_01j2".to_owned(),
                        occupancy_fencing_token: 7,
                        repository_binding_id: "rb_01j2".to_owned(),
                        product_session_id: "ps_01j2".to_owned(),
                        stage_run_id: None,
                        worker_session_id: "ws_01j2".to_owned(),
                        worker_id: "worker_1".to_owned(),
                        worker_instance_id: "winst_01j2".to_owned(),
                        credential_digest: "sha256:dd44".to_owned(),
                        expires_at: "2026-01-02T12:10:00Z".to_owned(),
                        state: WorkerLaunchGrantState::Issued,
                        revision: 1,
                    },
                }),
            ),
            (
                "client.worker.stop",
                ServerToClientMessage::WorkerStop(ServerWorkerStopPayload {
                    occupancy: occupancy.clone(),
                    worker_session_id: "ws_01j2".to_owned(),
                    reason: Some("release requested".to_owned()),
                }),
            ),
            (
                "client.candidate.apply",
                ServerToClientMessage::CandidateApply(ServerCandidateApplyPayload {
                    occupancy: occupancy.clone(),
                    candidate_ref: "cand_01j2".to_owned(),
                    repository_binding_id: "rb_01j2".to_owned(),
                    target_branch: "feature/x".to_owned(),
                    expected_head: Some("0123456789abcdef0123456789abcdef01234567".to_owned()),
                    strategy: ApplyStrategy::CherryPick,
                }),
            ),
            (
                "client.client_lock",
                ServerToClientMessage::ClientLock(ServerClientLockPayload {
                    command: command.clone(),
                    lock_state: ClientLockState::Locked,
                    reason: Some("administrative lock".to_owned()),
                }),
            ),
            (
                "client.credential_rotate",
                ServerToClientMessage::CredentialRotate(ServerCredentialRotatePayload {
                    command,
                    rotate_by: Some("2026-01-03T00:00:00Z".to_owned()),
                    reason: Some("suspected exposure".to_owned()),
                }),
            ),
        ]
    }

    #[test]
    fn every_client_to_server_kind_matches_the_plan_string() {
        for (expected_kind, message) in all_client_to_server_messages() {
            let value = serde_json::to_value(&message).expect("serialize message");
            assert_eq!(
                value.get("kind").and_then(serde_json::Value::as_str),
                Some(expected_kind),
                "kind string must match the plan exactly"
            );
            let reparsed: ClientToServerMessage =
                serde_json::from_value(value).expect("deserialize message");
            assert_eq!(reparsed, message, "message must round-trip");
        }
    }

    #[test]
    fn every_server_to_client_kind_matches_the_plan_string() {
        for (expected_kind, message) in all_server_to_client_messages() {
            let value = serde_json::to_value(&message).expect("serialize message");
            assert_eq!(
                value.get("kind").and_then(serde_json::Value::as_str),
                Some(expected_kind),
                "kind string must match the plan exactly"
            );
            let reparsed: ServerToClientMessage =
                serde_json::from_value(value).expect("deserialize message");
            assert_eq!(reparsed, message, "message must round-trip");
        }
    }

    #[test]
    fn envelope_carries_exactly_the_plan_frame_fields() {
        let (_, message) = all_client_to_server_messages()
            .into_iter()
            .find(|(kind, _)| *kind == "client.heartbeat")
            .expect("heartbeat entry");
        let envelope = ClientToServerEnvelope {
            schema_version: CLIENT_CONTROL_PORT_SCHEMA_VERSION,
            message_id: "msg_c2s_03".to_owned(),
            client_node_id: "node_01j2".to_owned(),
            client_instance_id: "inst_01j2".to_owned(),
            sequence: 2048,
            occurred_at: "2026-01-02T12:10:00Z".to_owned(),
            message,
        };
        let value = serde_json::to_value(&envelope).expect("serialize envelope");
        let mut keys: Vec<&str> = value
            .as_object()
            .expect("envelope must serialize to an object")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec![
                "clientInstanceId",
                "clientNodeId",
                "kind",
                "messageId",
                "occurredAt",
                "payload",
                "schemaVersion",
                "sequence"
            ]
        );
    }
}
