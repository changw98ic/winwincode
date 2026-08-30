// SPDX-License-Identifier: Apache-2.0

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use sha2::{Digest, Sha256};
use winwincode_control_plane::StorageEnterpriseUsageReconciler;
use winwincode_domain::{
    ArtifactId, DeliveryId, ExecutionJobId, ExecutionMessageId, FencingToken, LeaseId,
    OrganizationId, ProductSessionId, ProjectId, RepositoryId, RequestId, Sha256Digest, UserId,
    WorkerId, WorkerInstanceId, WorkerSessionId, WorkspaceId,
};
use winwincode_storage::{
    ArtifactChunk, ArtifactMeteringAttribution, ArtifactOpen, ArtifactProvenance,
    ArtifactRetention, ArtifactStore, EnterpriseUsageFilter, EnterpriseUsageSourceKind,
    FakeArtifactObjectStore, ReceiptScopeKey, SqliteStorage,
};

static NEXT_ROOT: AtomicU64 = AtomicU64::new(1);

fn temporary_directory() -> PathBuf {
    let seed = NEXT_ROOT.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "winwincode-enterprise-storage-{}-{seed}",
        std::process::id()
    ))
}

fn artifact_store(root: &std::path::Path) -> ArtifactStore {
    ArtifactStore::open(root, Box::new(FakeArtifactObjectStore::new())).expect("Artifact store")
}

fn seed_completed_artifact(artifacts: &mut ArtifactStore) {
    let bytes = b"durable enterprise storage bytes";
    let digest = Sha256Digest(format!("sha256:{:x}", Sha256::digest(bytes)));
    let scope = ReceiptScopeKey::from_encoded(b"repository:enterprise".to_vec()).expect("scope");
    let artifact_id = ArtifactId("art_00000000000000000000000701".into());
    let provenance = ArtifactProvenance::execution_job(
        ExecutionJobId("job_00000000000000000000000701".into()),
        1,
        LeaseId("lse_00000000000000000000000701".into()),
        FencingToken("701".into()),
        WorkerId("wrk_00000000000000000000000701".into()),
        WorkerInstanceId("wki_00000000000000000000000701".into()),
        WorkerSessionId("wsn_00000000000000000000000701".into()),
    )
    .expect("provenance");
    artifacts
        .open_artifact(ArtifactOpen::new(
            scope.clone(),
            ExecutionMessageId("xmsg_00000000000000000000000701".into()),
            RequestId("req_00000000000000000000000701".into()),
            artifact_id.clone(),
            "candidate",
            "application/octet-stream",
            digest.clone(),
            bytes.len() as u64,
            None,
            provenance.clone(),
            ArtifactMeteringAttribution {
                organization_id: OrganizationId("org_00000000000000000000000701".into()),
                workspace_id: WorkspaceId("wsp_00000000000000000000000701".into()),
                project_id: ProjectId("prj_00000000000000000000000701".into()),
                repository_id: RepositoryId("rep_00000000000000000000000701".into()),
                delivery_id: Some(DeliveryId("dlv_00000000000000000000000701".into())),
                product_session_id: Some(ProductSessionId("psn_00000000000000000000000701".into())),
                user_id: UserId("usr_00000000000000000000000701".into()),
            },
            ArtifactRetention::Indefinite,
            1_900_000_000_000,
        ))
        .expect("open");
    artifacts
        .append_chunk(&ArtifactChunk::new(
            scope,
            ExecutionMessageId("xmsg_00000000000000000000000702".into()),
            artifact_id,
            provenance,
            1_900_000_001_000,
            1,
            "application/octet-stream",
            digest,
            bytes.to_vec(),
            true,
        ))
        .expect("finalize");
}

#[test]
fn storage_sources_reconcile_once_and_replay_after_restart() {
    let root = temporary_directory();
    let state_root = root.join("state");
    let artifact_root = root.join("artifact");
    let mut artifacts = artifact_store(&artifact_root);
    seed_completed_artifact(&mut artifacts);
    let mut storage = SqliteStorage::open(&state_root).expect("storage");

    let first = StorageEnterpriseUsageReconciler::new(&mut storage, &artifacts)
        .reconcile_storage_page(None, 10)
        .expect("first reconciliation");
    assert_eq!(first.source_entries, 1);
    assert_eq!(first.inserted_entries, 1);
    assert_eq!(first.replayed_entries, 0);
    assert!(first.next.is_none());

    let replay = StorageEnterpriseUsageReconciler::new(&mut storage, &artifacts)
        .reconcile_storage_page(None, 10)
        .expect("same-process replay");
    assert_eq!(replay.inserted_entries, 0);
    assert_eq!(replay.replayed_entries, 1);
    artifacts.close().expect("Artifact close");
    drop(storage);

    let artifacts = artifact_store(&artifact_root);
    let mut storage = SqliteStorage::open(&state_root).expect("restart storage");
    let restarted = StorageEnterpriseUsageReconciler::new(&mut storage, &artifacts)
        .reconcile_storage_page(None, 10)
        .expect("restart replay");
    assert_eq!(restarted.inserted_entries, 0);
    assert_eq!(restarted.replayed_entries, 1);

    let filter = EnterpriseUsageFilter {
        organization_id: Some(OrganizationId("org_00000000000000000000000701".into())),
        workspace_id: Some(WorkspaceId("wsp_00000000000000000000000701".into())),
        project_id: Some(ProjectId("prj_00000000000000000000000701".into())),
        repository_id: Some(RepositoryId("rep_00000000000000000000000701".into())),
        delivery_id: Some(DeliveryId("dlv_00000000000000000000000701".into())),
        product_session_id: Some(ProductSessionId("psn_00000000000000000000000701".into())),
        user_id: Some(UserId("usr_00000000000000000000000701".into())),
        source_kind: Some(EnterpriseUsageSourceKind::Storage),
    };
    let page = storage
        .enterprise_usage_ledger()
        .expect("ledger")
        .scan(&filter, None, 10)
        .expect("ledger page");
    assert_eq!(page.entries.len(), 1);
    assert_eq!(
        page.entries[0].fact.attribution.product_session_id,
        filter.product_session_id
    );
    assert_eq!(
        page.entries[0].fact.measure,
        winwincode_storage::EnterpriseUsageMeasure::Storage {
            bytes: b"durable enterprise storage bytes".len() as u64,
        }
    );

    artifacts.close().expect("restart Artifact close");
    drop(storage);
    fs::remove_dir_all(root).expect("cleanup");
}
