// SPDX-License-Identifier: Apache-2.0

//! `ClientControlPort` envelope and message contract (plan section 9).
//!
//! Every message travels in an [`Envelope`] carrying the frame fields plus a
//! `kind`/`payload` pair. The field sets follow the authoritative schema
//! (`schema/winwincode/v1/client-control.schema.json`):
//!
//! - 19 command kinds embed the command fields (`expectedRevision`,
//!   `idempotencyKey`) flattened to the payload top level.
//! - 11 of those commands additionally embed the occupancy fencing stamp
//!   (`occupancyLeaseId`, `occupancyFencingToken`).
//! - The 8 remaining kinds (`client.hello`, `client.heartbeat`,
//!   `client.worker.state`, `client.worker.reconcile`,
//!   `client.repository.status`, `client.command_ack`,
//!   `client.enrollment_accepted`, `client.access.challenge`) carry neither
//!   the command fields nor a fencing token.
//!
//! The `kind` strings match the plan enumeration exactly, for example
//! `client.enroll` and `client.enrollment_accepted`. The envelope
//! `schemaVersion` is the domain contract string `"winwincode/v1"`, and
//! fencing tokens travel as decimal strings (see [`crate::wire`]).

#![allow(clippy::large_enum_variant)]

use serde::Deserialize;
use serde::Serialize;

use crate::domain::ApplyStrategy;
use crate::domain::ClientArchitecture;
use crate::domain::ClientCapacityReport;
use crate::domain::ClientChallengeAckStatus;
use crate::domain::ClientControlError;
use crate::domain::ClientControlMessageKind;
use crate::domain::ClientCredentialRotateReason;
use crate::domain::ClientLockState;
use crate::domain::ClientOccupancyForceFenceReason;
use crate::domain::ClientOccupancyReleaseMode;
use crate::domain::ClientPlatformTarget;
use crate::domain::ClientRepositoryRescanReason;
use crate::domain::ClientWorkerRunState;
use crate::domain::ClientWorkerStopReason;
use crate::domain::CommandAckStatus;
use crate::domain::LocalApplyReceipt;
use crate::domain::LocalCandidateReceipt;
use crate::domain::OccupancyRejectReason;
use crate::domain::PresenceState;
use crate::domain::RepositoryAvailability;
use crate::domain::RepositoryBindingProjection;
use crate::domain::RepositoryDirtyState;
use crate::domain::WorkerLaunchAckStatus;
use crate::domain::WorkerLaunchGrant;

/// Current `schemaVersion` of the `ClientControlPort` wire contract
/// (domain schema `SchemaVersion`).
pub const CLIENT_CONTROL_PORT_SCHEMA_VERSION: &str = "winwincode/v1";

/// Common command fields every command payload carries (plan section 9.5).
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
/// occupancy-, worker-, and candidate-related commands (plan section 9.5,
/// 12.6).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct OccupancyCommandContext {
    /// Common command fields.
    #[serde(flatten)]
    pub command: CommandContext,
    /// Occupancy lease the command is bound to.
    #[serde(rename = "occupancyLeaseId")]
    pub occupancy_lease_id: String,
    /// Fencing token of the occupancy lease; stale tokens are rejected.
    ///
    /// The wire encoding is a decimal string (schema `OccupancyFencingToken`).
    #[serde(rename = "occupancyFencingToken", with = "crate::wire::fencing_token")]
    pub occupancy_fencing_token: u64,
}

/// Envelope frame for `ClientControlPort` messages (plan section 9.5).
///
/// The `kind` and `payload` fields come from the flattened [`ClientToServerMessage`]
/// or [`ServerToClientMessage`] enum, so a serialized envelope has exactly the
/// contract's `schemaVersion`/`messageId`/`clientNodeId`/`clientInstanceId`/
/// `sequence`/`occurredAt`/`kind`/`payload` shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Envelope<M> {
    /// Wire contract schema version (`winwincode/v1`).
    #[serde(rename = "schemaVersion")]
    pub schema_version: String,
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
    /// Supported release target triple of the device.
    pub platform: ClientPlatformTarget,
    /// CPU architecture.
    pub architecture: ClientArchitecture,
    /// Device client software version.
    #[serde(rename = "clientVersion")]
    pub client_version: String,
}

/// Payload of `client.hello` (plan section 9.3).
///
/// A pure report: it carries no command context.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ClientHelloPayload {
    /// Device client software version.
    #[serde(rename = "clientVersion")]
    pub client_version: String,
    /// Worker session capacity report.
    pub capacity: ClientCapacityReport,
    /// Whether the node currently accepts new connections.
    #[serde(rename = "acceptingConnections")]
    pub accepting_connections: bool,
    /// Machine-level lock state.
    #[serde(rename = "lockState")]
    pub lock_state: ClientLockState,
    /// Machine-level presence state.
    #[serde(rename = "presenceState")]
    pub presence_state: PresenceState,
}

/// Payload of `client.heartbeat` (plan section 9.3).
///
/// A pure report: it carries no command context.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ClientHeartbeatPayload {
    /// Worker session capacity report.
    pub capacity: ClientCapacityReport,
    /// Whether the node currently accepts new connections.
    #[serde(rename = "acceptingConnections")]
    pub accepting_connections: bool,
    /// Machine-level lock state.
    #[serde(rename = "lockState")]
    pub lock_state: ClientLockState,
    /// Machine-level presence state.
    #[serde(rename = "presenceState")]
    pub presence_state: PresenceState,
    /// Locally mirrored active occupancy lease, if any.
    #[serde(rename = "occupancyLeaseId")]
    pub occupancy_lease_id: Option<String>,
}

/// Payload of `client.connect_code.published` (plan section 9.3, 11.3).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ClientConnectCodePublishedPayload {
    /// Common command fields.
    #[serde(flatten)]
    pub command: CommandContext,
    /// Connect code identifier.
    #[serde(rename = "connectCodeId")]
    pub connect_code_id: String,
    /// Digest of the code; the code itself never reaches the server.
    #[serde(rename = "codeDigest")]
    pub code_digest: String,
    /// Expiry timestamp (RFC 3339).
    #[serde(rename = "expiresAt")]
    pub expires_at: String,
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
    /// Connect code the challenge verified.
    #[serde(rename = "connectCodeId")]
    pub connect_code_id: String,
    /// Device verdict for the challenge.
    pub status: ClientChallengeAckStatus,
}

/// Payload of `client.occupancy.ack` (plan section 9.3, 12.2).
///
/// The acknowledgement carries no payload-specific fields: the fenced
/// command context is the whole payload.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ClientOccupancyAckPayload {
    /// Command fields with the occupancy fencing stamp.
    #[serde(flatten)]
    pub occupancy: OccupancyCommandContext,
}

/// Payload of `client.occupancy.rejected` (plan section 9.3, 12.6).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ClientOccupancyRejectedPayload {
    /// Command fields with the occupancy fencing stamp.
    #[serde(flatten)]
    pub occupancy: OccupancyCommandContext,
    /// Why the device rejected the occupancy.
    pub reason: OccupancyRejectReason,
}

/// Payload of `client.repository.upsert` (plan section 9.3, 13.1).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ClientRepositoryUpsertPayload {
    /// Common command fields.
    #[serde(flatten)]
    pub command: CommandContext,
    /// Secret-safe repository projection.
    #[serde(rename = "repository")]
    pub repository: RepositoryBindingProjection,
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
///
/// A pure report: it carries no command context.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ClientRepositoryStatusPayload {
    /// Binding the status refers to.
    #[serde(rename = "repositoryBindingId")]
    pub repository_binding_id: String,
    /// Availability projection.
    pub availability: RepositoryAvailability,
    /// Current HEAD commit.
    #[serde(rename = "headCommit")]
    pub head_commit: String,
    /// Dirty projection of the working tree.
    #[serde(rename = "dirtyState")]
    pub dirty_state: RepositoryDirtyState,
    /// Last scan timestamp (RFC 3339).
    #[serde(rename = "lastScannedAt")]
    pub last_scanned_at: String,
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
    /// Idempotent launch result.
    pub status: WorkerLaunchAckStatus,
    /// Machine-readable error fact, if the launch was rejected.
    pub error: Option<ClientControlError>,
}

/// Payload of `client.worker.state` (plan section 9.3).
///
/// A pure report: it carries no command context and no fencing token, only
/// the mirrored occupancy lease id.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ClientWorkerStatePayload {
    /// Mirrored occupancy lease, if any.
    #[serde(rename = "occupancyLeaseId")]
    pub occupancy_lease_id: Option<String>,
    /// Worker session the state refers to.
    #[serde(rename = "workerSessionId")]
    pub worker_session_id: String,
    /// Worker instance identity.
    #[serde(rename = "workerInstanceId")]
    pub worker_instance_id: String,
    /// Device-observed process state.
    pub state: ClientWorkerRunState,
    /// Process exit code, if the worker exited.
    #[serde(rename = "exitCode")]
    pub exit_code: Option<i32>,
    /// Observation timestamp (RFC 3339).
    #[serde(rename = "observedAt")]
    pub observed_at: String,
}

/// One reconciled worker process in `client.worker.reconcile` (schema
/// `ClientWorkerReconciliation`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientWorkerReconciliation {
    /// Worker session the entry refers to.
    #[serde(rename = "workerSessionId")]
    pub worker_session_id: String,
    /// Worker instance identity.
    #[serde(rename = "workerInstanceId")]
    pub worker_instance_id: String,
    /// Reconciliation verdict for the process.
    #[serde(rename = "reconcileState")]
    pub reconcile_state: crate::domain::WorkerReconcileState,
    /// Observation timestamp (RFC 3339).
    #[serde(rename = "observedAt")]
    pub observed_at: String,
}

/// Payload of `client.worker.reconcile` (plan section 9.3, 12.5).
///
/// A pure report: it carries no command context and no fencing token, only
/// the mirrored occupancy lease id.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ClientWorkerReconcilePayload {
    /// Mirrored occupancy lease under recovery, if any.
    #[serde(rename = "occupancyLeaseId")]
    pub occupancy_lease_id: Option<String>,
    /// Locally observed worker processes after reconnection.
    pub workers: Vec<ClientWorkerReconciliation>,
}

/// Payload of `client.candidate.retained` (plan section 9.3, 15.2).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ClientCandidateRetainedPayload {
    /// Command fields with the occupancy fencing stamp.
    #[serde(flatten)]
    pub occupancy: OccupancyCommandContext,
    /// Worker session that froze the candidate.
    #[serde(rename = "workerSessionId")]
    pub worker_session_id: String,
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
///
/// A universal acknowledgement: it carries no command context of its own.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ClientCommandAckPayload {
    /// Kind of the server command being acknowledged.
    #[serde(rename = "commandKind")]
    pub command_kind: ClientControlMessageKind,
    /// Message id of the server command being acknowledged.
    #[serde(rename = "commandMessageId")]
    pub command_message_id: String,
    /// Universal acknowledgement status.
    pub status: CommandAckStatus,
    /// Server-side revision after the command, when accepted.
    #[serde(rename = "currentRevision")]
    pub current_revision: Option<u64>,
    /// Machine-readable error fact, if the command was rejected.
    pub error: Option<ClientControlError>,
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
///
/// A pure response: it carries no command context.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ServerEnrollmentAcceptedPayload {
    /// Stable public device identifier, not a secret.
    #[serde(rename = "publicClientId")]
    pub public_client_id: String,
    /// Requested heartbeat interval in milliseconds.
    #[serde(rename = "heartbeatIntervalMs")]
    pub heartbeat_interval_ms: u32,
    /// Server timestamp the device should clock-drift against (RFC 3339).
    #[serde(rename = "serverTime")]
    pub server_time: String,
}

/// Payload of `client.access.challenge` (plan section 9.4, 11.4).
///
/// A pure request: it carries no command context.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ServerAccessChallengePayload {
    /// Challenge identifier.
    #[serde(rename = "challengeId")]
    pub challenge_id: String,
    /// Connect code being verified.
    #[serde(rename = "connectCodeId")]
    pub connect_code_id: String,
    /// Digest of the connect code.
    #[serde(rename = "codeDigest")]
    pub code_digest: String,
    /// Expiry timestamp (RFC 3339).
    #[serde(rename = "expiresAt")]
    pub expires_at: String,
    /// User the challenge was issued to.
    #[serde(rename = "requesterUserId")]
    pub requester_user_id: String,
}

/// Payload of `client.occupancy.offer` (plan section 9.4, 12.2).
///
/// The new lease's identity and fencing fields are flattened into the fenced
/// command context; the claim facts ride next to them.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ServerOccupancyOfferPayload {
    /// Command fields with the occupancy fencing stamp of the new lease.
    #[serde(flatten)]
    pub occupancy: OccupancyCommandContext,
    /// Identifier of the claim request that created the lease.
    #[serde(rename = "claimRequestId")]
    pub claim_request_id: String,
    /// Claim timestamp (RFC 3339).
    #[serde(rename = "claimedAt")]
    pub claimed_at: String,
    /// Occupying user.
    #[serde(rename = "holderUserId")]
    pub holder_user_id: String,
    /// Idle expiry timestamp (RFC 3339), if bounded.
    #[serde(rename = "idleExpiresAt")]
    pub idle_expires_at: Option<String>,
}

/// Payload of `client.occupancy.release` (plan section 9.4, 12.4).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ServerOccupancyReleasePayload {
    /// Command fields with the occupancy fencing stamp.
    #[serde(flatten)]
    pub occupancy: OccupancyCommandContext,
    /// Requested release behavior.
    pub mode: ClientOccupancyReleaseMode,
}

/// Payload of `client.occupancy.force_fence` (plan section 9.4, 12.6).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ServerOccupancyForceFencePayload {
    /// Command fields with the new occupancy fencing stamp.
    #[serde(flatten)]
    pub occupancy: OccupancyCommandContext,
    /// Why the lease is being force-fenced.
    pub reason: ClientOccupancyForceFenceReason,
    /// Superseded lease, if the fence replaces an older lease.
    #[serde(rename = "supersededLeaseId")]
    pub superseded_lease_id: Option<String>,
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
    /// Why the rescan is requested.
    pub reason: ClientRepositoryRescanReason,
}

/// Payload of `client.worker.launch` (plan section 9.4, 14.3).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ServerWorkerLaunchPayload {
    /// Command fields with the occupancy fencing stamp.
    #[serde(flatten)]
    pub occupancy: OccupancyCommandContext,
    /// Single-use launch grant to consume.
    #[serde(rename = "launchGrant")]
    pub launch_grant: WorkerLaunchGrant,
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
    /// Worker identity.
    #[serde(rename = "workerId")]
    pub worker_id: String,
    /// Why the worker is stopped.
    pub reason: ClientWorkerStopReason,
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
    /// Expected target HEAD before the apply.
    #[serde(rename = "expectedHead")]
    pub expected_head: String,
    /// User the apply is performed for.
    #[serde(rename = "requesterUserId")]
    pub requester_user_id: String,
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
}

/// Payload of `client.credential_rotate` (plan section 9.4).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ServerCredentialRotatePayload {
    /// Common command fields.
    #[serde(flatten)]
    pub command: CommandContext,
    /// Why the rotation is requested.
    pub reason: ClientCredentialRotateReason,
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
    use crate::domain::ClientArchitecture as Architecture;
    use crate::domain::LocalCandidateState;
    use crate::domain::RepositoryKind;
    use crate::domain::WorkerLaunchGrantState;
    use crate::domain::WorkerReconcileState;

    /// Golden fixture of a plain command kind, straight from the
    /// repository's `tests/fixtures/client-control` lane.
    const RESCAN_FIXTURE: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/fixtures/client-control/client.repository.rescan.json"
    ));

    /// Golden fixture of the pure-report kind.
    const HEARTBEAT_FIXTURE: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/fixtures/client-control/client.heartbeat.json"
    ));

    /// Golden fixture of a fencing-stamped command kind.
    const CANDIDATE_RETAINED_FIXTURE: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/fixtures/client-control/client.candidate.retained.json"
    ));

    /// Command context fields the schema keeps out of the 8 non-command
    /// kinds; `occupancyLeaseId` is handled separately because the three
    /// reports mirror it (nullable).
    const COMMAND_CONTEXT_FIELDS: [&str; 3] = [
        "expectedRevision",
        "idempotencyKey",
        "occupancyFencingToken",
    ];

    /// Kinds whose payload flattens an [`OccupancyCommandContext`] to the
    /// payload top level (schema `x-message-class: command` with fencing).
    const FENCED_KINDS: [&str; 11] = [
        "client.occupancy.ack",
        "client.occupancy.rejected",
        "client.occupancy.offer",
        "client.occupancy.release",
        "client.occupancy.force_fence",
        "client.worker.launch",
        "client.worker.launch_ack",
        "client.worker.stop",
        "client.candidate.retained",
        "client.candidate.apply_result",
        "client.candidate.apply",
    ];

    /// Kinds that carry neither command fields nor a fencing token.
    const CONTEXT_FREE_KINDS: [&str; 8] = [
        "client.hello",
        "client.heartbeat",
        "client.worker.state",
        "client.worker.reconcile",
        "client.repository.status",
        "client.command_ack",
        "client.enrollment_accepted",
        "client.access.challenge",
    ];

    /// The context-free reports that mirror the occupancy lease id (nullable,
    /// schema-required) without carrying a fencing token.
    const LEASE_MIRRORING_REPORTS: [&str; 3] = [
        "client.heartbeat",
        "client.worker.state",
        "client.worker.reconcile",
    ];

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

    fn capacity() -> ClientCapacityReport {
        ClientCapacityReport {
            max_concurrent_worker_sessions: 3,
            running_worker_sessions: 1,
            reserved_worker_sessions: 1,
            draining_worker_sessions: 0,
        }
    }

    fn candidate_receipt() -> LocalCandidateReceipt {
        LocalCandidateReceipt {
            local_candidate_receipt_id: "lcr_01j2".to_owned(),
            candidate_ref: "cand_01j2".to_owned(),
            repository_binding_id: "rb_01j2".to_owned(),
            candidate_commit: "89abcdef0123456789abcdef0123456789abcdef".to_owned(),
            local_ref_name: "refs/winwincode/candidates/cand_01j2".to_owned(),
            state: LocalCandidateState::Retained,
            created_at: "2026-01-02T12:30:00.000Z".to_owned(),
            revision: 1,
        }
    }

    fn apply_receipt() -> LocalApplyReceipt {
        LocalApplyReceipt {
            local_apply_receipt_id: "lar_01j2".to_owned(),
            candidate_ref: "cand_01j2".to_owned(),
            repository_binding_id: "rb_01j2".to_owned(),
            target_branch: "feature/x".to_owned(),
            expected_head: "0123456789abcdef0123456789abcdef01234567".to_owned(),
            strategy: ApplyStrategy::CherryPick,
            result: ApplyResult::Applied,
            resulting_commit: Some("fedcba9876543210fedcba9876543210fedcba98".to_owned()),
            conflict_artifact_ref: None,
            created_at: "2026-01-02T12:31:00.000Z".to_owned(),
            revision: 2,
        }
    }

    fn launch_grant() -> WorkerLaunchGrant {
        WorkerLaunchGrant {
            worker_launch_grant_id: "wlg_01j2".to_owned(),
            client_node_id: "node_01j2".to_owned(),
            client_instance_id: "inst_01j2".to_owned(),
            occupancy_lease_id: "lease_01j2".to_owned(),
            occupancy_fencing_token: 7,
            repository_binding_id: "rb_01j2".to_owned(),
            product_session_id: "ps_01j2".to_owned(),
            stage_run_id: "stg_01j2".to_owned(),
            worker_session_id: "ws_01j2".to_owned(),
            worker_id: "worker_1".to_owned(),
            worker_instance_id: "winst_01j2".to_owned(),
            credential_digest: "sha256:dd44".to_owned(),
            expires_at: "2026-01-02T12:10:00.000Z".to_owned(),
            state: WorkerLaunchGrantState::Issued,
            revision: 1,
        }
    }

    fn repository_projection() -> RepositoryBindingProjection {
        RepositoryBindingProjection {
            repository_binding_id: "rb_01j2".to_owned(),
            display_name: "winwincode".to_owned(),
            repository_kind: RepositoryKind::Git,
            default_branch: "main".to_owned(),
            head_commit: "0123456789abcdef0123456789abcdef01234567".to_owned(),
            dirty_state: RepositoryDirtyState::Clean,
            availability: RepositoryAvailability::Available,
            repository_fingerprint: "sha256:cc33".to_owned(),
            last_scanned_at: "2026-01-02T11:00:00.000Z".to_owned(),
        }
    }

    fn server_envelope_for(message: ServerToClientMessage) -> ServerToClientEnvelope {
        ServerToClientEnvelope {
            schema_version: CLIENT_CONTROL_PORT_SCHEMA_VERSION.to_owned(),
            message_id: "msg_s2c_fixture".to_owned(),
            client_node_id: "node_01j2".to_owned(),
            client_instance_id: "inst_01j2".to_owned(),
            sequence: 1,
            occurred_at: "2026-01-02T12:00:00.000Z".to_owned(),
            message,
        }
    }

    /// Recursively reports whether `value` carries `field` at any depth.
    fn carries_field(value: &serde_json::Value, field: &str) -> bool {
        match value {
            serde_json::Value::Object(map) => {
                map.contains_key(field) || map.values().any(|child| carries_field(child, field))
            }
            serde_json::Value::Array(items) => {
                items.iter().any(|child| carries_field(child, field))
            }
            _ => false,
        }
    }

    fn sorted_keys(value: &serde_json::Value) -> Vec<&str> {
        let mut keys: Vec<&str> = value
            .as_object()
            .expect("value must be an object")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        keys
    }

    #[test]
    fn occupancy_ack_envelope_round_trips_with_fencing_fields() {
        let json = r#"{
            "schemaVersion": "winwincode/v1",
            "messageId": "msg_c2s_01",
            "clientNodeId": "node_01j2",
            "clientInstanceId": "inst_01j2",
            "sequence": 1024,
            "occurredAt": "2026-01-02T12:00:01.000Z",
            "kind": "client.occupancy.ack",
            "payload": {
                "expectedRevision": 41,
                "idempotencyKey": "idem_01j2",
                "occupancyLeaseId": "lease_01j2",
                "occupancyFencingToken": "7"
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
    fn occupancy_ack_rejects_a_non_string_fencing_token() {
        let json = r#"{
            "schemaVersion": "winwincode/v1",
            "messageId": "msg_c2s_01",
            "clientNodeId": "node_01j2",
            "clientInstanceId": "inst_01j2",
            "sequence": 1024,
            "occurredAt": "2026-01-02T12:00:01.000Z",
            "kind": "client.occupancy.ack",
            "payload": {
                "expectedRevision": 41,
                "idempotencyKey": "idem_01j2",
                "occupancyLeaseId": "lease_01j2",
                "occupancyFencingToken": 7
            }
        }"#;
        let parsed: Result<ClientToServerEnvelope, _> = serde_json::from_str(json);
        assert!(
            parsed.is_err(),
            "a numeric occupancyFencingToken must not deserialize"
        );
    }

    #[test]
    fn worker_launch_envelope_round_trips() {
        let json = r#"{
            "schemaVersion": "winwincode/v1",
            "messageId": "msg_s2c_01",
            "clientNodeId": "node_01j2",
            "clientInstanceId": "inst_01j2",
            "sequence": 512,
            "occurredAt": "2026-01-02T12:05:00.000Z",
            "kind": "client.worker.launch",
            "payload": {
                "expectedRevision": 41,
                "idempotencyKey": "idem_s2c_01",
                "occupancyLeaseId": "lease_01j2",
                "occupancyFencingToken": "7",
                "launchGrant": {
                    "workerLaunchGrantId": "wlg_01j2",
                    "clientNodeId": "node_01j2",
                    "clientInstanceId": "inst_01j2",
                    "occupancyLeaseId": "lease_01j2",
                    "occupancyFencingToken": "7",
                    "repositoryBindingId": "rb_01j2",
                    "productSessionId": "ps_01j2",
                    "stageRunId": "stg_01j2",
                    "workerSessionId": "ws_01j2",
                    "workerId": "worker_1",
                    "workerInstanceId": "winst_01j2",
                    "credentialDigest": "sha256:dd44",
                    "expiresAt": "2026-01-02T12:10:00.000Z",
                    "state": "issued",
                    "revision": 1
                }
            }
        }"#;
        let envelope = assert_round_trip::<ServerToClientEnvelope>(json);
        let ServerToClientMessage::WorkerLaunch(payload) = &envelope.message else {
            panic!("expected client.worker.launch");
        };
        assert_eq!(payload.launch_grant.worker_session_id, "ws_01j2");
        assert_eq!(payload.occupancy.occupancy_fencing_token, 7);
        assert_eq!(payload.launch_grant.occupancy_fencing_token, 7);
    }

    #[test]
    fn enroll_envelope_round_trips_with_boxed_payload() {
        let json = r#"{
            "schemaVersion": "winwincode/v1",
            "messageId": "msg_c2s_02",
            "clientNodeId": "node_01j2",
            "clientInstanceId": "inst_01j2",
            "sequence": 1,
            "occurredAt": "2026-01-01T00:00:00.000Z",
            "kind": "client.enroll",
            "payload": {
                "expectedRevision": 0,
                "idempotencyKey": "idem_enroll_01",
                "displayName": "Cheng's MacBook",
                "platform": "aarch64-apple-darwin",
                "architecture": "aarch64",
                "clientVersion": "0.1.0-alpha.1"
            }
        }"#;
        let envelope = assert_round_trip::<ClientToServerEnvelope>(json);
        let ClientToServerMessage::Enroll(payload) = &envelope.message else {
            panic!("expected client.enroll");
        };
        assert_eq!(payload.display_name, "Cheng's MacBook");
        assert_eq!(payload.architecture, Architecture::Aarch64);
        assert_eq!(envelope.schema_version, "winwincode/v1");
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
                    platform: ClientPlatformTarget::Aarch64AppleDarwin,
                    architecture: Architecture::Aarch64,
                    client_version: "0.1.0-alpha.1".to_owned(),
                })),
            ),
            (
                "client.hello",
                ClientToServerMessage::Hello(ClientHelloPayload {
                    client_version: "0.1.0-alpha.1".to_owned(),
                    capacity: capacity(),
                    accepting_connections: true,
                    lock_state: ClientLockState::Unlocked,
                    presence_state: PresenceState::Online,
                }),
            ),
            (
                "client.heartbeat",
                ClientToServerMessage::Heartbeat(ClientHeartbeatPayload {
                    capacity: capacity(),
                    accepting_connections: true,
                    lock_state: ClientLockState::Unlocked,
                    presence_state: PresenceState::Online,
                    occupancy_lease_id: Some("lease_01j2".to_owned()),
                }),
            ),
            (
                "client.connect_code.published",
                ClientToServerMessage::ConnectCodePublished(ClientConnectCodePublishedPayload {
                    command: command.clone(),
                    connect_code_id: "code_01j2".to_owned(),
                    code_digest: "sha256:bb22".to_owned(),
                    expires_at: "2026-01-01T01:00:00.000Z".to_owned(),
                }),
            ),
            (
                "client.access.challenge_ack",
                ClientToServerMessage::AccessChallengeAck(Box::new(
                    ClientAccessChallengeAckPayload {
                        command: command.clone(),
                        challenge_id: "chal_01j2".to_owned(),
                        connect_code_id: "code_01j2".to_owned(),
                        status: ClientChallengeAckStatus::Confirmed,
                    },
                )),
            ),
            (
                "client.occupancy.ack",
                ClientToServerMessage::OccupancyAck(ClientOccupancyAckPayload {
                    occupancy: occupancy.clone(),
                }),
            ),
            (
                "client.occupancy.rejected",
                ClientToServerMessage::OccupancyRejected(ClientOccupancyRejectedPayload {
                    occupancy: occupancy.clone(),
                    reason: OccupancyRejectReason::StaleFencingToken,
                }),
            ),
            (
                "client.repository.upsert",
                ClientToServerMessage::RepositoryUpsert(ClientRepositoryUpsertPayload {
                    command: command.clone(),
                    repository: repository_projection(),
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
                    repository_binding_id: "rb_01j2".to_owned(),
                    availability: RepositoryAvailability::Available,
                    head_commit: "0123456789abcdef0123456789abcdef01234567".to_owned(),
                    dirty_state: RepositoryDirtyState::Clean,
                    last_scanned_at: "2026-01-02T11:00:00.000Z".to_owned(),
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
                    status: WorkerLaunchAckStatus::Accepted,
                    error: None,
                })),
            ),
            (
                "client.worker.state",
                ClientToServerMessage::WorkerState(ClientWorkerStatePayload {
                    occupancy_lease_id: Some("lease_01j2".to_owned()),
                    worker_session_id: "ws_01j2".to_owned(),
                    worker_instance_id: "winst_01j2".to_owned(),
                    state: ClientWorkerRunState::Running,
                    exit_code: None,
                    observed_at: "2026-01-02T12:06:00.000Z".to_owned(),
                }),
            ),
            (
                "client.worker.reconcile",
                ClientToServerMessage::WorkerReconcile(ClientWorkerReconcilePayload {
                    occupancy_lease_id: Some("lease_01j2".to_owned()),
                    workers: vec![ClientWorkerReconciliation {
                        worker_session_id: "ws_01j2".to_owned(),
                        worker_instance_id: "winst_01j2".to_owned(),
                        reconcile_state: WorkerReconcileState::StillRunning,
                        observed_at: "2026-01-02T12:06:00.000Z".to_owned(),
                    }],
                }),
            ),
            (
                "client.candidate.retained",
                ClientToServerMessage::CandidateRetained(ClientCandidateRetainedPayload {
                    occupancy: occupancy.clone(),
                    worker_session_id: "ws_01j2".to_owned(),
                    receipt: candidate_receipt(),
                }),
            ),
            (
                "client.candidate.apply_result",
                ClientToServerMessage::CandidateApplyResult(ClientCandidateApplyResultPayload {
                    occupancy: occupancy.clone(),
                    receipt: apply_receipt(),
                }),
            ),
            (
                "client.command_ack",
                ClientToServerMessage::CommandAck(ClientCommandAckPayload {
                    command_kind: ClientControlMessageKind::WorkerLaunch,
                    command_message_id: "msg_s2c_01".to_owned(),
                    status: CommandAckStatus::Accepted,
                    current_revision: Some(42),
                    error: None,
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
                    public_client_id: "100200300401".to_owned(),
                    heartbeat_interval_ms: 15_000,
                    server_time: "2026-01-01T00:00:00.000Z".to_owned(),
                }),
            ),
            (
                "client.access.challenge",
                ServerToClientMessage::AccessChallenge(Box::new(ServerAccessChallengePayload {
                    challenge_id: "chal_01j2".to_owned(),
                    connect_code_id: "code_01j2".to_owned(),
                    code_digest: "sha256:bb22".to_owned(),
                    expires_at: "2026-01-01T01:00:00.000Z".to_owned(),
                    requester_user_id: "usr_01j2".to_owned(),
                })),
            ),
            (
                "client.occupancy.offer",
                ServerToClientMessage::OccupancyOffer(ServerOccupancyOfferPayload {
                    occupancy: occupancy.clone(),
                    claim_request_id: "claim_01j2".to_owned(),
                    claimed_at: "2026-01-02T12:00:00.000Z".to_owned(),
                    holder_user_id: "usr_01j2".to_owned(),
                    idle_expires_at: Some("2026-01-02T14:00:00.000Z".to_owned()),
                }),
            ),
            (
                "client.occupancy.release",
                ServerToClientMessage::OccupancyRelease(ServerOccupancyReleasePayload {
                    occupancy: occupancy.clone(),
                    mode: ClientOccupancyReleaseMode::DrainThenRelease,
                }),
            ),
            (
                "client.occupancy.force_fence",
                ServerToClientMessage::OccupancyForceFence(ServerOccupancyForceFencePayload {
                    occupancy: occupancy.clone(),
                    reason: ClientOccupancyForceFenceReason::RecoveryDeadlineExceeded,
                    superseded_lease_id: Some("lease_old".to_owned()),
                }),
            ),
            (
                "client.repository.rescan",
                ServerToClientMessage::RepositoryRescan(ServerRepositoryRescanPayload {
                    command: command.clone(),
                    repository_binding_id: "rb_01j2".to_owned(),
                    reason: ClientRepositoryRescanReason::OccupantRequested,
                }),
            ),
            (
                "client.worker.launch",
                ServerToClientMessage::WorkerLaunch(ServerWorkerLaunchPayload {
                    occupancy: occupancy.clone(),
                    launch_grant: launch_grant(),
                }),
            ),
            (
                "client.worker.stop",
                ServerToClientMessage::WorkerStop(ServerWorkerStopPayload {
                    occupancy: occupancy.clone(),
                    worker_session_id: "ws_01j2".to_owned(),
                    worker_id: "worker_1".to_owned(),
                    reason: ClientWorkerStopReason::OccupantRequested,
                }),
            ),
            (
                "client.candidate.apply",
                ServerToClientMessage::CandidateApply(ServerCandidateApplyPayload {
                    occupancy: occupancy.clone(),
                    candidate_ref: "cand_01j2".to_owned(),
                    repository_binding_id: "rb_01j2".to_owned(),
                    target_branch: "feature/x".to_owned(),
                    expected_head: "0123456789abcdef0123456789abcdef01234567".to_owned(),
                    requester_user_id: "usr_01j2".to_owned(),
                    strategy: ApplyStrategy::CherryPick,
                }),
            ),
            (
                "client.client_lock",
                ServerToClientMessage::ClientLock(ServerClientLockPayload {
                    command: command.clone(),
                    lock_state: ClientLockState::Locked,
                }),
            ),
            (
                "client.credential_rotate",
                ServerToClientMessage::CredentialRotate(ServerCredentialRotatePayload {
                    command,
                    reason: ClientCredentialRotateReason::SuspectedCompromise,
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
    fn command_and_fencing_contexts_follow_the_schema_classification() {
        assert_eq!(FENCED_KINDS.len(), 11, "exactly 11 fenced kinds");
        assert_eq!(CONTEXT_FREE_KINDS.len(), 8, "exactly 8 context-free kinds");

        let messages: Vec<(&str, serde_json::Value)> = all_client_to_server_messages()
            .into_iter()
            .map(|(kind, message)| {
                let value = serde_json::to_value(&message).expect("serialize message");
                (kind, value)
            })
            .chain(
                all_server_to_client_messages()
                    .into_iter()
                    .map(|(kind, message)| {
                        let value = serde_json::to_value(&message).expect("serialize message");
                        (kind, value)
                    }),
            )
            .collect();
        assert_eq!(messages.len(), 27, "every protocol kind is represented");

        // 19 commands per the schema's x-message-class, 11 of them fenced.
        let plain_commands = 27 - FENCED_KINDS.len() - CONTEXT_FREE_KINDS.len();
        assert_eq!(plain_commands, 8, "exactly 8 plain command kinds");
        assert_eq!(
            FENCED_KINDS.len() + plain_commands,
            19,
            "11 fenced + 8 plain = the 19 command kinds"
        );

        for (kind, value) in &messages {
            let payload = value.get("payload").expect("message carries a payload");
            let fenced = FENCED_KINDS.contains(kind);
            let context_free = CONTEXT_FREE_KINDS.contains(kind);

            // Command fields: present for the 19 commands, absent for the
            // other 8 kinds.
            assert_eq!(
                payload.get("expectedRevision").is_some(),
                !context_free,
                "{kind}: expectedRevision placement"
            );
            assert_eq!(
                payload.get("idempotencyKey").is_some(),
                !context_free,
                "{kind}: idempotencyKey placement"
            );

            // The fencing token rides only on the 11 fenced kinds. The
            // context-free reports may mirror occupancyLeaseId but never a
            // token.
            assert_eq!(
                payload.get("occupancyFencingToken").is_some(),
                fenced,
                "{kind}: occupancyFencingToken placement"
            );
            if fenced {
                assert!(payload.get("occupancyLeaseId").is_some(), "{kind}");
                let token = payload.get("occupancyFencingToken").expect("token");
                assert!(
                    token.is_string(),
                    "{kind}: the fencing token must be a JSON string"
                );
            }

            if context_free {
                for field in COMMAND_CONTEXT_FIELDS {
                    assert!(
                        !carries_field(value, field),
                        "{kind}: context-free kinds must not carry {field}"
                    );
                }
                // The three occupancy-mirroring reports carry a nullable
                // occupancyLeaseId; the other context-free kinds do not.
                let mirrors_lease = LEASE_MIRRORING_REPORTS.contains(kind);
                assert_eq!(
                    carries_field(value, "occupancyLeaseId"),
                    mirrors_lease,
                    "{kind}: occupancyLeaseId placement"
                );
            }
        }
    }

    #[test]
    fn envelope_carries_exactly_the_plan_frame_fields() {
        let (_, message) = all_client_to_server_messages()
            .into_iter()
            .find(|(kind, _)| *kind == "client.heartbeat")
            .expect("heartbeat entry");
        let envelope = ClientToServerEnvelope {
            schema_version: CLIENT_CONTROL_PORT_SCHEMA_VERSION.to_owned(),
            message_id: "msg_c2s_03".to_owned(),
            client_node_id: "node_01j2".to_owned(),
            client_instance_id: "inst_01j2".to_owned(),
            sequence: 2048,
            occurred_at: "2026-01-02T12:10:00.000Z".to_owned(),
            message,
        };
        let value = serde_json::to_value(&envelope).expect("serialize envelope");
        assert_eq!(
            sorted_keys(&value),
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
        assert_eq!(
            value
                .get("schemaVersion")
                .and_then(serde_json::Value::as_str),
            Some("winwincode/v1"),
            "schemaVersion is the domain contract string"
        );
    }

    /// The command fixture (client.repository.rescan) and this crate must
    /// present the same field collection: the frame fields, the command
    /// context, and no fencing fields.
    #[test]
    fn command_fixture_field_collection_aligns() {
        let fixture: serde_json::Value =
            serde_json::from_str(RESCAN_FIXTURE).expect("parse rescan fixture");

        // The fixture's demo-era payload body predates the authoritative
        // schema def (it carries rescanId/requestedByUserId and a
        // since-narrowed reason enum); the schema def owns payload contents.
        // The frame, the command context, and the binding id are shared by
        // both sources and must align value-for-value.
        let expected_revision = fixture
            .get("expectedRevision")
            .and_then(serde_json::Value::as_u64)
            .expect("fixture carries expectedRevision");
        let idempotency_key = fixture
            .get("idempotencyKey")
            .and_then(serde_json::Value::as_str)
            .expect("fixture carries idempotencyKey");
        let repository_binding_id = fixture
            .get("payload")
            .and_then(|payload| payload.get("repositoryBindingId"))
            .and_then(serde_json::Value::as_str)
            .expect("fixture payload carries repositoryBindingId");

        let message = ServerToClientMessage::RepositoryRescan(ServerRepositoryRescanPayload {
            command: CommandContext {
                expected_revision,
                idempotency_key: idempotency_key.to_owned(),
            },
            repository_binding_id: repository_binding_id.to_owned(),
            reason: ClientRepositoryRescanReason::OccupantRequested,
        });
        let envelope = server_envelope_for(message);
        let mine = serde_json::to_value(&envelope).expect("serialize envelope");

        // The fixture frames the message exactly like this crate; the
        // command context fields ride at the fixture's envelope level and at
        // this crate's payload top level (flattened command context).
        let context = ["expectedRevision", "idempotencyKey"];
        let fixture_frame: Vec<&str> = sorted_keys(&fixture)
            .into_iter()
            .filter(|key| !context.contains(key))
            .collect();
        assert_eq!(
            fixture_frame,
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
        assert_eq!(sorted_keys(&mine), fixture_frame, "frame fields align");

        assert_eq!(
            fixture
                .get("schemaVersion")
                .and_then(serde_json::Value::as_str),
            Some("winwincode/v1")
        );
        assert_eq!(
            mine.get("schemaVersion")
                .and_then(serde_json::Value::as_str),
            fixture
                .get("schemaVersion")
                .and_then(serde_json::Value::as_str)
        );
        assert_eq!(mine.get("kind"), fixture.get("kind"));

        for field in ["occupancyLeaseId", "occupancyFencingToken"] {
            assert!(
                fixture.get(field).is_none(),
                "the rescan fixture must not carry {field}"
            );
            assert!(
                !carries_field(&mine, field),
                "the serialized rescan must not carry {field}"
            );
        }

        let payload = mine.get("payload").expect("payload");
        assert_eq!(
            payload.get("expectedRevision"),
            fixture.get("expectedRevision"),
            "expectedRevision value and JSON type align"
        );
        assert_eq!(
            payload.get("idempotencyKey"),
            fixture.get("idempotencyKey"),
            "idempotencyKey value aligns"
        );
        assert_eq!(
            payload.get("repositoryBindingId"),
            fixture
                .get("payload")
                .and_then(|body| body.get("repositoryBindingId")),
            "repositoryBindingId value aligns"
        );
    }

    #[test]
    fn report_fixture_carries_no_context_fields() {
        let fixture: serde_json::Value =
            serde_json::from_str(HEARTBEAT_FIXTURE).expect("parse heartbeat fixture");
        for field in COMMAND_CONTEXT_FIELDS {
            assert!(
                fixture.get(field).is_none(),
                "the heartbeat fixture must not carry {field}"
            );
        }
        assert!(
            fixture.get("occupancyLeaseId").is_none(),
            "the heartbeat fixture predates the nullable lease mirror"
        );

        let (_, message) = all_client_to_server_messages()
            .into_iter()
            .find(|(kind, _)| *kind == "client.heartbeat")
            .expect("heartbeat entry");
        let envelope = ClientToServerEnvelope {
            schema_version: CLIENT_CONTROL_PORT_SCHEMA_VERSION.to_owned(),
            message_id: "msg_c2s_03".to_owned(),
            client_node_id: "node_01j2".to_owned(),
            client_instance_id: "inst_01j2".to_owned(),
            sequence: 403,
            occurred_at: "2026-01-02T12:00:00.000Z".to_owned(),
            message,
        };
        let mine = serde_json::to_value(&envelope).expect("serialize envelope");
        for field in COMMAND_CONTEXT_FIELDS {
            assert!(
                !carries_field(&mine, field),
                "the serialized heartbeat must not carry {field}"
            );
        }
        // The schema def requires the nullable lease mirror even though the
        // demo-era fixture omits it; it must serialize (as null or a lease
        // id), never as a fencing token.
        assert!(
            mine.get("payload")
                .expect("payload")
                .get("occupancyLeaseId")
                .is_some_and(|lease| lease.is_null() || lease.is_string()),
            "the heartbeat payload mirrors occupancyLeaseId as null or a lease id"
        );
        assert_eq!(mine.get("kind"), fixture.get("kind"));
    }

    /// The fenced fixture (client.candidate.retained) and this crate must
    /// agree on the fencing collection: lease id plus a decimal-string
    /// fencing token next to the command fields.
    #[test]
    fn fenced_fixture_field_collection_aligns() {
        let fixture: serde_json::Value =
            serde_json::from_str(CANDIDATE_RETAINED_FIXTURE).expect("parse retained fixture");

        let token = fixture
            .get("occupancyFencingToken")
            .and_then(serde_json::Value::as_str)
            .expect("fixture carries a decimal-string occupancyFencingToken");
        let lease_id = fixture
            .get("occupancyLeaseId")
            .and_then(serde_json::Value::as_str)
            .expect("fixture carries occupancyLeaseId");
        let expected_revision = fixture
            .get("expectedRevision")
            .and_then(serde_json::Value::as_u64)
            .expect("fixture carries expectedRevision");
        let idempotency_key = fixture
            .get("idempotencyKey")
            .and_then(serde_json::Value::as_str)
            .expect("fixture carries idempotencyKey");

        let message = ClientToServerMessage::CandidateRetained(ClientCandidateRetainedPayload {
            occupancy: OccupancyCommandContext {
                command: CommandContext {
                    expected_revision,
                    idempotency_key: idempotency_key.to_owned(),
                },
                occupancy_lease_id: lease_id.to_owned(),
                occupancy_fencing_token: crate::wire::fencing_token::parse_token(token)
                    .expect("fixture token parses"),
            },
            worker_session_id: "wks_000000000000000000000001".to_owned(),
            receipt: candidate_receipt(),
        });
        let mine = serde_json::to_value(&message).expect("serialize message");

        let payload = mine.get("payload").expect("payload");
        assert_eq!(
            payload.get("occupancyFencingToken"),
            fixture.get("occupancyFencingToken"),
            "the fencing token serializes as the same decimal string"
        );
        assert_eq!(
            payload.get("occupancyLeaseId"),
            fixture.get("occupancyLeaseId")
        );
        assert_eq!(
            payload.get("expectedRevision"),
            fixture.get("expectedRevision")
        );
        assert_eq!(payload.get("idempotencyKey"), fixture.get("idempotencyKey"));
        assert_eq!(
            payload
                .get("occupancyFencingToken")
                .and_then(serde_json::Value::as_str),
            Some("3"),
            "the fixture pins token 3 as a JSON string"
        );
        assert_eq!(
            mine.get("kind"),
            fixture.get("kind"),
            "kind strings align with the fixture filename"
        );
    }
}
