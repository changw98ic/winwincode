// SPDX-License-Identifier: Apache-2.0

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use sha2::{Digest, Sha256};
use winwincode_control_plane::{
    ArtifactEnterpriseQuotaAdmission, ArtifactEnterpriseQuotaSaga,
    ArtifactEnterpriseQuotaSagaError, DurableArtifactEnterpriseUsage,
    DurableEnterpriseQuotaAdmission,
};
use winwincode_domain::{
    ArtifactId, DeliveryId, ExecutionJobId, ExecutionMessageId, FencingToken, Instant, LeaseId,
    OrganizationId, ProductSessionId, ProjectId, RepositoryId, RequestId, Sha256Digest, UserId,
    WorkerId, WorkerInstanceId, WorkerSessionId, WorkspaceId,
};
use winwincode_storage::{
    ArtifactChunk, ArtifactMeteringAttribution, ArtifactOpen, ArtifactProvenance,
    ArtifactRetention, ArtifactStorageOperationKind, ArtifactStore, EnterpriseQuotaBoundary,
    EnterpriseQuotaLimits, EnterpriseQuotaPolicy, EnterpriseQuotaReleaseReason,
    EnterpriseQuotaReservationReceipt, EnterpriseQuotaReservationState, EnterpriseQuotaSourceSeal,
    FakeArtifactObjectStore, ReceiptScopeKey, SqliteStorage,
};

static NEXT_ROOT: AtomicU64 = AtomicU64::new(1);

fn temporary_directory(name: &str) -> PathBuf {
    let seed = NEXT_ROOT.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "winwincode-artifact-enterprise-quota-{name}-{}-{seed}",
        std::process::id()
    ))
}

fn id(prefix: &str, value: u64) -> String {
    format!("{prefix}_{value:026}")
}

fn attribution() -> ArtifactMeteringAttribution {
    ArtifactMeteringAttribution {
        organization_id: OrganizationId(id("org", 1)),
        workspace_id: WorkspaceId(id("wsp", 2)),
        project_id: ProjectId(id("prj", 3)),
        repository_id: RepositoryId(id("rep", 4)),
        delivery_id: Some(DeliveryId(id("dlv", 5))),
        product_session_id: Some(ProductSessionId(id("psn", 6))),
        user_id: UserId(id("usr", 7)),
    }
}

fn artifact_store(root: &std::path::Path) -> ArtifactStore {
    ArtifactStore::open(root, Box::new(FakeArtifactObjectStore::new())).expect("Artifact store")
}

fn open(bytes: &[u8], sequence: u64, created_at_millis: u64) -> ArtifactOpen {
    ArtifactOpen::new(
        ReceiptScopeKey::from_encoded(b"repository:artifact-enterprise".to_vec()).expect("scope"),
        ExecutionMessageId(id("xmsg", sequence)),
        RequestId(id("req", sequence)),
        ArtifactId(id("art", sequence)),
        "candidate",
        "application/octet-stream",
        Sha256Digest(format!("sha256:{:x}", Sha256::digest(bytes))),
        bytes.len() as u64,
        None,
        ArtifactProvenance::execution_job(
            ExecutionJobId(id("job", sequence)),
            1,
            LeaseId(id("lse", sequence)),
            FencingToken(sequence.to_string()),
            WorkerId(id("wrk", sequence)),
            WorkerInstanceId(id("wki", sequence)),
            WorkerSessionId(id("wsn", sequence)),
        )
        .expect("provenance"),
        attribution(),
        ArtifactRetention::Indefinite,
        created_at_millis,
    )
}

fn finalize(artifacts: &mut ArtifactStore, open: &ArtifactOpen, bytes: &[u8], sequence: u64) {
    let receipt = artifacts
        .open_artifact(open.clone())
        .expect("open Artifact");
    artifacts
        .append_chunk(&ArtifactChunk::new(
            ReceiptScopeKey::from_encoded(b"repository:artifact-enterprise".to_vec())
                .expect("scope"),
            ExecutionMessageId(id("xmsg", sequence + 1)),
            open.artifact_id().clone(),
            receipt.record().provenance().clone(),
            1_900_000_001_000,
            1,
            "application/octet-stream",
            Sha256Digest(format!("sha256:{:x}", Sha256::digest(bytes))),
            bytes.to_vec(),
            true,
        ))
        .expect("finalize Artifact");
}

fn configure_policy(admission: &mut DurableEnterpriseQuotaAdmission, max_storage: u64) {
    admission
        .put_policy(&EnterpriseQuotaPolicy {
            boundary: EnterpriseQuotaBoundary::Organization {
                organization_id: attribution().organization_id,
            },
            revision: 1,
            limits: EnterpriseQuotaLimits {
                storage_bytes: Some(max_storage),
                ..EnterpriseQuotaLimits::default()
            },
        })
        .expect("policy");
}

fn release_unfinished_artifact_job(
    quota: &mut DurableEnterpriseQuotaAdmission,
    usage: &mut DurableArtifactEnterpriseUsage,
    artifacts: &ArtifactStore,
) -> EnterpriseQuotaReservationReceipt {
    ArtifactEnterpriseQuotaSaga::new(quota, usage)
        .release_unfinished_job(
            artifacts,
            &ExecutionJobId(id("job", 202)),
            EnterpriseQuotaReleaseReason::Cancelled,
            &Instant("2030-03-02T10:00:01.000Z".into()),
        )
        .expect("release unfinished Job")
        .pop()
        .expect("released receipt")
}

#[test]
fn artifact_saga_seals_open_then_settles_only_the_immutable_final_storage_source() {
    let root = temporary_directory("settle");
    let state_root = root.join("state");
    let artifact_root = root.join("artifact");
    let bytes = b"immutable artifact quota bytes";
    let open = open(bytes, 101, 1_898_589_600_000);
    let mut artifacts = artifact_store(&artifact_root);
    let mut quota = DurableEnterpriseQuotaAdmission::new(
        SqliteStorage::open(&state_root).expect("quota storage"),
    );
    configure_policy(&mut quota, bytes.len() as u64);
    let mut usage = DurableArtifactEnterpriseUsage::new(
        SqliteStorage::open(&state_root).expect("usage storage"),
    );

    let reservation = {
        let mut saga = ArtifactEnterpriseQuotaSaga::new(&mut quota, &mut usage);
        let ArtifactEnterpriseQuotaAdmission::Admitted(reservation) = saga
            .reserve_open(&open, &Instant("2030-03-01T10:00:00.000Z".into()))
            .expect("open quota admission")
        else {
            panic!("expected Artifact quota admission");
        };
        assert!(matches!(
            reservation.receipt().record.source_seal,
            EnterpriseQuotaSourceSeal::Storage {
                operation_kind: ArtifactStorageOperationKind::ArtifactFinalize,
                expected_bytes,
                ..
            } if expected_bytes == bytes.len() as u64
        ));
        let error = saga
            .settle_final(&reservation, &artifacts)
            .expect_err("final source must exist before settlement");
        assert!(matches!(
            error,
            ArtifactEnterpriseQuotaSagaError::MissingFinalStorageSource
        ));
        reservation
    };
    finalize(&mut artifacts, &open, bytes, 101);
    let final_source = artifacts
        .storage_source_for_artifact(open.artifact_id())
        .expect("read source")
        .expect("completed source");
    assert_eq!(final_source.fact.request_id, *open.request_id());
    assert_eq!(final_source.fact.bytes, bytes.len() as u64);

    let settled = {
        let mut saga = ArtifactEnterpriseQuotaSaga::new(&mut quota, &mut usage);
        saga.settle_final(&reservation, &artifacts)
            .expect("settle final source")
    };
    assert_eq!(
        settled.record.state,
        EnterpriseQuotaReservationState::Settled
    );
    assert_eq!(settled.record.revision, 2);
    let replayed_settlement = {
        let mut saga = ArtifactEnterpriseQuotaSaga::new(&mut quota, &mut usage);
        saga.settle_final(&reservation, &artifacts)
            .expect("settlement replay")
    };
    assert!(replayed_settlement.idempotent_replay);

    quota.close().expect("quota close");
    usage.close().expect("usage close");
    let mut restarted_quota = DurableEnterpriseQuotaAdmission::new(
        SqliteStorage::open(&state_root).expect("restart quota storage"),
    );
    let mut restarted_usage = DurableArtifactEnterpriseUsage::new(
        SqliteStorage::open(&state_root).expect("restart usage storage"),
    );
    let replay = ArtifactEnterpriseQuotaSaga::new(&mut restarted_quota, &mut restarted_usage)
        .reserve_open(&open, &Instant("2030-03-01T10:00:00.000Z".into()))
        .expect("restart reserve replay");
    assert!(matches!(
        replay,
        ArtifactEnterpriseQuotaAdmission::TerminalReplay(receipt)
            if receipt.record.state == EnterpriseQuotaReservationState::Settled
    ));
    restarted_quota.close().expect("restart quota close");
    restarted_usage.close().expect("restart usage close");
    artifacts.close().expect("Artifact close");
    fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn artifact_saga_denies_before_storage_write_and_releases_cancelled_reservations_stably() {
    let root = temporary_directory("release");
    let state_root = root.join("state");
    let artifact_root = root.join("artifact");
    let bytes = b"artifact bytes";
    let denied_open = open(bytes, 201, 1_898_676_000_000);
    let released_open = open(bytes, 202, 1_898_676_000_000);
    let mut artifacts = artifact_store(&artifact_root);
    let mut quota = DurableEnterpriseQuotaAdmission::new(
        SqliteStorage::open(&state_root).expect("quota storage"),
    );
    configure_policy(&mut quota, bytes.len() as u64 - 1);
    let mut usage = DurableArtifactEnterpriseUsage::new(
        SqliteStorage::open(&state_root).expect("usage storage"),
    );

    let denied = ArtifactEnterpriseQuotaSaga::new(&mut quota, &mut usage)
        .reserve_open(&denied_open, &Instant("2030-03-02T10:00:00.000Z".into()))
        .expect("denial result");
    assert!(matches!(
        denied,
        ArtifactEnterpriseQuotaAdmission::Denied(_)
    ));
    assert!(
        artifacts
            .storage_source_for_artifact(denied_open.artifact_id())
            .expect("read denied source")
            .is_none()
    );

    quota
        .put_policy(&EnterpriseQuotaPolicy {
            boundary: EnterpriseQuotaBoundary::Organization {
                organization_id: attribution().organization_id,
            },
            revision: 2,
            limits: EnterpriseQuotaLimits {
                storage_bytes: Some(bytes.len() as u64),
                ..EnterpriseQuotaLimits::default()
            },
        })
        .expect("wider policy");
    let reservation = {
        let mut saga = ArtifactEnterpriseQuotaSaga::new(&mut quota, &mut usage);
        let ArtifactEnterpriseQuotaAdmission::Admitted(reservation) = saga
            .reserve_open(&released_open, &Instant("2030-03-02T10:00:00.000Z".into()))
            .expect("admission")
        else {
            panic!("expected Artifact quota admission");
        };
        reservation
    };
    assert_eq!(
        reservation.receipt().record.reservation_id,
        *released_open.request_id()
    );
    artifacts
        .open_artifact(released_open.clone())
        .expect("open unfinished Artifact");
    let released = release_unfinished_artifact_job(&mut quota, &mut usage, &artifacts);
    assert_eq!(
        released.record.state,
        EnterpriseQuotaReservationState::Released
    );
    let replayed_release = release_unfinished_artifact_job(&mut quota, &mut usage, &artifacts);
    assert!(replayed_release.idempotent_replay);

    quota.close().expect("quota close");
    usage.close().expect("usage close");
    let mut restarted_quota = DurableEnterpriseQuotaAdmission::new(
        SqliteStorage::open(&state_root).expect("restart quota storage"),
    );
    let mut restarted_usage = DurableArtifactEnterpriseUsage::new(
        SqliteStorage::open(&state_root).expect("restart usage storage"),
    );
    let replay = ArtifactEnterpriseQuotaSaga::new(&mut restarted_quota, &mut restarted_usage)
        .reserve_open(&released_open, &Instant("2030-03-02T10:00:00.000Z".into()))
        .expect("restart reserve replay");
    assert!(matches!(
        replay,
        ArtifactEnterpriseQuotaAdmission::TerminalReplay(receipt)
            if receipt.record.state == EnterpriseQuotaReservationState::Released
    ));
    restarted_quota.close().expect("restart quota close");
    restarted_usage.close().expect("restart usage close");
    artifacts.close().expect("Artifact close");
    fs::remove_dir_all(root).expect("cleanup");
}
