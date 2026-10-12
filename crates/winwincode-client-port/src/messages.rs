#![allow(clippy::large_enum_variant)]
// SPDX-License-Identifier: Apache-2.0

use serde::Deserialize;
use serde::Serialize;

use crate::domain::ApplyStrategy;
use crate::domain::ClientArchitecture;
use crate::domain::ClientCapacityReport;
use crate::domain::ClientControlError;
use crate::domain::ClientControlMessageKind;
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
use crate::managed_app::{ManagedAppCommand, ManagedAppStatus};

pub const CLIENT_CONTROL_PORT_SCHEMA_VERSION: &str = "winwincode/v1";

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandContext {
    #[serde(rename = "expectedRevision")]
    pub expected_revision: u64,
    #[serde(rename = "idempotencyKey")]
    pub idempotency_key: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct OccupancyCommandContext {
    #[serde(flatten)]
    pub command: CommandContext,
    #[serde(rename = "occupancyLeaseId")]
    pub occupancy_lease_id: String,
    #[serde(rename = "occupancyFencingToken", with = "crate::wire::fencing_token")]
    pub occupancy_fencing_token: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Envelope<M> {
    #[serde(rename = "schemaVersion")]
    pub schema_version: String,
    #[serde(rename = "messageId")]
    pub message_id: String,
    #[serde(rename = "clientNodeId")]
    pub client_node_id: String,
    #[serde(rename = "clientInstanceId")]
    pub client_instance_id: String,
    pub sequence: u64,
    #[serde(rename = "occurredAt")]
    pub occurred_at: String,
    #[serde(flatten)]
    pub message: M,
}

pub type ClientToServerEnvelope = Envelope<ClientToServerMessage>;

pub type ServerToClientEnvelope = Envelope<ServerToClientMessage>;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ClientEnrollPayload {
    #[serde(flatten)]
    pub command: CommandContext,
    #[serde(rename = "displayName")]
    pub display_name: String,
    pub platform: ClientPlatformTarget,
    pub architecture: ClientArchitecture,
    #[serde(rename = "clientVersion")]
    pub client_version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ClientHelloPayload {
    #[serde(rename = "clientVersion")]
    pub client_version: String,
    pub capacity: ClientCapacityReport,
    #[serde(rename = "acceptingConnections")]
    pub accepting_connections: bool,
    #[serde(rename = "lockState")]
    pub lock_state: ClientLockState,
    #[serde(rename = "presenceState")]
    pub presence_state: PresenceState,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ClientHeartbeatPayload {
    pub capacity: ClientCapacityReport,
    #[serde(rename = "acceptingConnections")]
    pub accepting_connections: bool,
    #[serde(rename = "lockState")]
    pub lock_state: ClientLockState,
    #[serde(rename = "presenceState")]
    pub presence_state: PresenceState,
    #[serde(rename = "occupancyLeaseId")]
    pub occupancy_lease_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ClientConnectCodePublishedPayload {
    #[serde(flatten)]
    pub command: CommandContext,
    #[serde(rename = "connectCodeId")]
    pub connect_code_id: String,
    #[serde(rename = "codeDigest")]
    pub code_digest: String,
    pub generation: u64,
    #[serde(rename = "expiresAt")]
    pub expires_at: String,
    #[serde(rename = "remainingAttempts")]
    pub remaining_attempts: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ClientOccupancyAckPayload {
    #[serde(flatten)]
    pub occupancy: OccupancyCommandContext,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ClientOccupancyRejectedPayload {
    #[serde(flatten)]
    pub occupancy: OccupancyCommandContext,
    pub reason: OccupancyRejectReason,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ClientRepositoryUpsertPayload {
    #[serde(flatten)]
    pub command: CommandContext,
    #[serde(rename = "repository")]
    pub repository: RepositoryBindingProjection,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ClientRepositoryRemovedPayload {
    #[serde(flatten)]
    pub command: CommandContext,
    #[serde(rename = "repositoryBindingId")]
    pub repository_binding_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ClientRepositoryStatusPayload {
    #[serde(rename = "repositoryBindingId")]
    pub repository_binding_id: String,
    pub availability: RepositoryAvailability,
    #[serde(rename = "headCommit")]
    pub head_commit: String,
    #[serde(rename = "dirtyState")]
    pub dirty_state: RepositoryDirtyState,
    #[serde(rename = "lastScannedAt")]
    pub last_scanned_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ClientWorkerLaunchAckPayload {
    #[serde(flatten)]
    pub occupancy: OccupancyCommandContext,
    #[serde(rename = "workerLaunchGrantId")]
    pub worker_launch_grant_id: String,
    #[serde(rename = "workerSessionId")]
    pub worker_session_id: String,
    #[serde(rename = "workerId")]
    pub worker_id: String,
    #[serde(rename = "workerInstanceId")]
    pub worker_instance_id: String,
    pub status: WorkerLaunchAckStatus,
    pub error: Option<ClientControlError>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "processClosure"
    )]
    pub process_closure: Option<ClientWorkerProcessClosure>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ClientWorkerProcessClosure {
    pub worker_launch_grant_id: String,
    pub client_instance_id: String,
    pub reporting_client_instance_id: String,
    #[serde(with = "crate::wire::fencing_token")]
    pub occupancy_fencing_token: u64,
    pub never_started: bool,
    pub process_boot_digest: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ClientWorkerStatePayload {
    #[serde(rename = "occupancyLeaseId")]
    pub occupancy_lease_id: Option<String>,
    #[serde(rename = "workerSessionId")]
    pub worker_session_id: String,
    #[serde(rename = "workerInstanceId")]
    pub worker_instance_id: String,
    pub state: ClientWorkerRunState,
    #[serde(rename = "exitCode")]
    pub exit_code: Option<i32>,
    #[serde(rename = "observedAt")]
    pub observed_at: String,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "processClosure"
    )]
    pub process_closure: Option<ClientWorkerProcessClosure>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientWorkerReconciliation {
    #[serde(rename = "workerSessionId")]
    pub worker_session_id: String,
    #[serde(rename = "workerInstanceId")]
    pub worker_instance_id: String,
    #[serde(rename = "reconcileState")]
    pub reconcile_state: crate::domain::WorkerReconcileState,
    #[serde(rename = "observedAt")]
    pub observed_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ClientWorkerReconcilePayload {
    #[serde(rename = "occupancyLeaseId")]
    pub occupancy_lease_id: Option<String>,
    pub workers: Vec<ClientWorkerReconciliation>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ClientCandidateRetainedPayload {
    #[serde(flatten)]
    pub occupancy: OccupancyCommandContext,
    #[serde(rename = "workerSessionId")]
    pub worker_session_id: String,
    #[serde(rename = "receipt")]
    pub receipt: LocalCandidateReceipt,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ClientCandidateApplyResultPayload {
    #[serde(flatten)]
    pub occupancy: OccupancyCommandContext,
    #[serde(rename = "receipt")]
    pub receipt: LocalApplyReceipt,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ClientCommandAckPayload {
    #[serde(rename = "commandKind")]
    pub command_kind: ClientControlMessageKind,
    #[serde(rename = "commandMessageId")]
    pub command_message_id: String,
    pub status: CommandAckStatus,
    #[serde(rename = "currentRevision", skip_serializing_if = "Option::is_none")]
    pub current_revision: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ClientControlError>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ClientManagedAppStatusPayload {
    #[serde(rename = "commandMessageId")]
    pub command_message_id: String,
    pub status: ManagedAppStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ServerManagedAppCommandPayload {
    pub command: ManagedAppCommand,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "payload")]
pub enum ClientToServerMessage {
    #[serde(rename = "client.repository.registered")]
    RepositoryRegistered {
        receipt: winwincode_api::generated::DeviceRepositoryRegistrationReceipt,
    },
    #[serde(rename = "client.extension.report")]
    ExtensionReport(Box<winwincode_api::generated::DeviceExtensionReport>),
    #[serde(rename = "client.provider.report")]
    ProviderReport(Box<winwincode_api::generated::DeviceProviderReport>),
    #[serde(rename = "client.enroll")]
    Enroll(Box<ClientEnrollPayload>),
    #[serde(rename = "client.hello")]
    Hello(ClientHelloPayload),
    #[serde(rename = "client.heartbeat")]
    Heartbeat(ClientHeartbeatPayload),
    #[serde(rename = "client.connect_code.published")]
    ConnectCodePublished(ClientConnectCodePublishedPayload),
    #[serde(rename = "client.occupancy.ack")]
    OccupancyAck(ClientOccupancyAckPayload),
    #[serde(rename = "client.occupancy.rejected")]
    OccupancyRejected(ClientOccupancyRejectedPayload),
    #[serde(rename = "client.repository.upsert")]
    RepositoryUpsert(ClientRepositoryUpsertPayload),
    #[serde(rename = "client.repository.removed")]
    RepositoryRemoved(ClientRepositoryRemovedPayload),
    #[serde(rename = "client.repository.status")]
    RepositoryStatus(ClientRepositoryStatusPayload),
    #[serde(rename = "client.worker.launch_ack")]
    WorkerLaunchAck(Box<ClientWorkerLaunchAckPayload>),
    #[serde(rename = "client.worker.state")]
    WorkerState(ClientWorkerStatePayload),
    #[serde(rename = "client.worker.reconcile")]
    WorkerReconcile(ClientWorkerReconcilePayload),
    #[serde(rename = "client.candidate.retained")]
    CandidateRetained(ClientCandidateRetainedPayload),
    #[serde(rename = "client.candidate.apply_result")]
    CandidateApplyResult(ClientCandidateApplyResultPayload),
    #[serde(rename = "client.command_ack")]
    CommandAck(ClientCommandAckPayload),
    #[serde(rename = "client.managed_app.status")]
    ManagedAppStatus(ClientManagedAppStatusPayload),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ServerOccupancyOfferPayload {
    #[serde(flatten)]
    pub occupancy: OccupancyCommandContext,
    #[serde(rename = "claimRequestId")]
    pub claim_request_id: String,
    #[serde(rename = "claimedAt")]
    pub claimed_at: String,
    #[serde(rename = "holderUserId")]
    pub holder_user_id: String,
    #[serde(rename = "idleExpiresAt")]
    pub idle_expires_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ServerOccupancyReleasePayload {
    #[serde(flatten)]
    pub occupancy: OccupancyCommandContext,
    pub mode: ClientOccupancyReleaseMode,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ServerOccupancyForceFencePayload {
    #[serde(flatten)]
    pub occupancy: OccupancyCommandContext,
    pub reason: ClientOccupancyForceFenceReason,
    #[serde(rename = "supersededLeaseId")]
    pub superseded_lease_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ServerRepositoryRescanPayload {
    #[serde(flatten)]
    pub command: CommandContext,
    #[serde(rename = "repositoryBindingId")]
    pub repository_binding_id: String,
    pub reason: ClientRepositoryRescanReason,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ServerWorkerLaunchPayload {
    #[serde(flatten)]
    pub occupancy: OccupancyCommandContext,
    #[serde(rename = "launchGrant")]
    pub launch_grant: WorkerLaunchGrant,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ServerWorkerStopPayload {
    #[serde(flatten)]
    pub occupancy: OccupancyCommandContext,
    #[serde(rename = "workerSessionId")]
    pub worker_session_id: String,
    #[serde(rename = "workerId")]
    pub worker_id: String,
    pub reason: ClientWorkerStopReason,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ServerCandidateApplyPayload {
    #[serde(flatten)]
    pub occupancy: OccupancyCommandContext,
    #[serde(rename = "candidateRef")]
    pub candidate_ref: String,
    #[serde(rename = "repositoryBindingId")]
    pub repository_binding_id: String,
    #[serde(rename = "targetBranch")]
    pub target_branch: String,
    #[serde(rename = "expectedHead")]
    pub expected_head: String,
    #[serde(rename = "requesterUserId")]
    pub requester_user_id: String,
    pub strategy: ApplyStrategy,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ServerClientLockPayload {
    #[serde(flatten)]
    pub command: CommandContext,
    #[serde(rename = "lockState")]
    pub lock_state: ClientLockState,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ServerConfigurationApplyPayload {
    #[serde(flatten)]
    pub command: CommandContext,
    pub encrypted: winwincode_api::generated::DeviceConfigurationEnvelope,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "payload")]
pub enum ServerToClientMessage {
    #[serde(rename = "client.command_ack")]
    CommandAck(ClientCommandAckPayload),
    #[serde(rename = "client.repository.register")]
    RepositoryRegister(Box<ServerConfigurationApplyPayload>),
    #[serde(rename = "client.extension.apply")]
    ExtensionApply(Box<ServerConfigurationApplyPayload>),
    #[serde(rename = "client.provider.apply")]
    ProviderApply(Box<ServerConfigurationApplyPayload>),
    #[serde(rename = "client.occupancy.offer")]
    OccupancyOffer(ServerOccupancyOfferPayload),
    #[serde(rename = "client.occupancy.release")]
    OccupancyRelease(ServerOccupancyReleasePayload),
    #[serde(rename = "client.occupancy.force_fence")]
    OccupancyForceFence(ServerOccupancyForceFencePayload),
    #[serde(rename = "client.repository.rescan")]
    RepositoryRescan(ServerRepositoryRescanPayload),
    #[serde(rename = "client.worker.launch")]
    WorkerLaunch(ServerWorkerLaunchPayload),
    #[serde(rename = "client.worker.stop")]
    WorkerStop(ServerWorkerStopPayload),
    #[serde(rename = "client.candidate.apply")]
    CandidateApply(ServerCandidateApplyPayload),
    #[serde(rename = "client.client_lock")]
    ClientLock(ServerClientLockPayload),
    #[serde(rename = "client.managed_app.command")]
    ManagedAppCommand(ServerManagedAppCommandPayload),
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

    const RESCAN_FIXTURE: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/fixtures/client-control/client.repository.rescan.json"
    ));

    const HEARTBEAT_FIXTURE: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/fixtures/client-control/client.heartbeat.json"
    ));

    const CANDIDATE_RETAINED_FIXTURE: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/fixtures/client-control/client.candidate.retained.json"
    ));

    const COMMAND_CONTEXT_FIELDS: [&str; 3] = [
        "expectedRevision",
        "idempotencyKey",
        "occupancyFencingToken",
    ];

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

    const CONTEXT_FREE_KINDS: [&str; 6] = [
        "client.hello",
        "client.heartbeat",
        "client.worker.state",
        "client.worker.reconcile",
        "client.repository.status",
        "client.command_ack",
    ];

    const LEASE_MIRRORING_REPORTS: [&str; 3] = [
        "client.heartbeat",
        "client.worker.state",
        "client.worker.reconcile",
    ];

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

    #[test]
    fn process_closure_preserves_full_u64_fencing_precision_on_wire() {
        let closure = ClientWorkerProcessClosure {
            worker_launch_grant_id: "wlg_00000000000000000000000001".into(),
            client_instance_id: "cix_00000000000000000000000001".into(),
            reporting_client_instance_id: "cix_00000000000000000000000002".into(),
            occupancy_fencing_token: u64::MAX,
            never_started: true,
            process_boot_digest: None,
        };
        let wire = serde_json::to_value(&closure).unwrap();
        assert_eq!(wire["occupancyFencingToken"], u64::MAX.to_string());
        assert_eq!(
            serde_json::from_value::<ClientWorkerProcessClosure>(wire.clone()).unwrap(),
            closure
        );
        for invalid in [
            serde_json::json!(u64::MAX),
            serde_json::json!("0"),
            serde_json::json!("01"),
            serde_json::json!("18446744073709551616"),
        ] {
            let mut wrong = wire.clone();
            wrong["occupancyFencingToken"] = invalid;
            assert!(serde_json::from_value::<ClientWorkerProcessClosure>(wrong).is_err());
        }
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
            work_run_id: Some("wrn_01j2".to_owned()),
            worker_session_id: "wsn_01j2".to_owned(),
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
                    "workRunId": "wrn_01j2",
                    "workerSessionId": "wsn_01j2",
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
        assert_eq!(payload.launch_grant.worker_session_id, "wsn_01j2");
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
                    generation: 1,
                    expires_at: "2026-01-01T01:00:00.000Z".to_owned(),
                    remaining_attempts: 5,
                }),
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
                    process_closure: None,
                    occupancy: occupancy.clone(),
                    worker_launch_grant_id: "wlg_01j2".to_owned(),
                    worker_session_id: "wsn_01j2".to_owned(),
                    worker_id: "worker_1".to_owned(),
                    worker_instance_id: "winst_01j2".to_owned(),
                    status: WorkerLaunchAckStatus::Accepted,
                    error: None,
                })),
            ),
            (
                "client.worker.state",
                ClientToServerMessage::WorkerState(ClientWorkerStatePayload {
                    process_closure: None,
                    occupancy_lease_id: Some("lease_01j2".to_owned()),
                    worker_session_id: "wsn_01j2".to_owned(),
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
                        worker_session_id: "wsn_01j2".to_owned(),
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
                    worker_session_id: "wsn_01j2".to_owned(),
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

    #[allow(clippy::too_many_lines)]
    fn all_server_to_client_messages() -> Vec<(&'static str, ServerToClientMessage)> {
        let command = command_context();
        let occupancy = occupancy_context();
        vec![
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
                    worker_session_id: "wsn_01j2".to_owned(),
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
        assert_eq!(CONTEXT_FREE_KINDS.len(), 6, "exactly 6 context-free kinds");

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
        assert_eq!(messages.len(), 23, "every fixture kind is represented");

        let plain_commands = messages.len() - FENCED_KINDS.len() - CONTEXT_FREE_KINDS.len();
        assert_eq!(plain_commands, 6, "exactly 6 plain command kinds");
        assert_eq!(
            FENCED_KINDS.len() + plain_commands,
            17,
            "11 fenced + 6 plain = the 17 command kinds"
        );

        for (kind, value) in &messages {
            let payload = value.get("payload").expect("message carries a payload");
            let fenced = FENCED_KINDS.contains(kind);
            let context_free = CONTEXT_FREE_KINDS.contains(kind);

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

    #[test]
    fn command_fixture_field_collection_aligns() {
        let fixture: serde_json::Value =
            serde_json::from_str(RESCAN_FIXTURE).expect("parse rescan fixture");

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
        assert!(
            mine.get("payload")
                .expect("payload")
                .get("occupancyLeaseId")
                .is_some_and(|lease| lease.is_null() || lease.is_string()),
            "the heartbeat payload mirrors occupancyLeaseId as null or a lease id"
        );
        assert_eq!(mine.get("kind"), fixture.get("kind"));
    }

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
