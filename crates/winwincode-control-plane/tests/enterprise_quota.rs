use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use winwincode_control_plane::{
    DurableEnterpriseQuotaAdmission, EnterpriseQuotaAdmission, EnterpriseQuotaAdmissionPort,
};
use winwincode_domain::{
    Instant, ModelExchangeId, OrganizationId, ProductSessionId, ProjectId, RepositoryId, RequestId,
    UserId, WorkspaceId,
};
use winwincode_storage::{
    EnterpriseQuotaAmounts, EnterpriseQuotaBoundary, EnterpriseQuotaDimension,
    EnterpriseQuotaLimits, EnterpriseQuotaPolicy, EnterpriseQuotaRelease,
    EnterpriseQuotaReleaseReason, EnterpriseQuotaReservationRequest, EnterpriseQuotaSourceSeal,
    EnterpriseUsageAttribution, SqliteStorage,
};

static NEXT_TEMP_DIRECTORY: AtomicU64 = AtomicU64::new(1);

fn temporary_directory(name: &str) -> PathBuf {
    let suffix = NEXT_TEMP_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "winwincode-control-plane-enterprise-quota-{name}-{}-{suffix}",
        std::process::id()
    ))
}

fn id(prefix: &str, value: u64) -> String {
    format!("{prefix}_{value:026}")
}

fn attribution() -> EnterpriseUsageAttribution {
    EnterpriseUsageAttribution {
        organization_id: OrganizationId(id("org", 1)),
        workspace_id: WorkspaceId(id("wsp", 2)),
        project_id: ProjectId(id("prj", 3)),
        repository_id: RepositoryId(id("rep", 4)),
        delivery_id: None,
        product_session_id: Some(ProductSessionId(id("psn", 5))),
        user_id: UserId(id("usr", 6)),
    }
}

fn request(seed: u64) -> EnterpriseQuotaReservationRequest {
    EnterpriseQuotaReservationRequest {
        reservation_id: RequestId(id("req", seed)),
        attribution: attribution(),
        source_seal: EnterpriseQuotaSourceSeal::Provider {
            model_exchange_id: ModelExchangeId(id("mdl", seed)),
            request_id: RequestId(id("req", seed + 100)),
            attempt: 1,
            route_authority_fingerprint: format!("sha256:{seed:064x}"),
        },
        reserved: EnterpriseQuotaAmounts {
            tokens: 100,
            provider_cost_micros: 10,
            operations: 1,
            ..EnterpriseQuotaAmounts::default()
        },
        requested_at: Instant("2027-05-01T08:00:00.000Z".to_owned()),
    }
}

#[test]
fn durable_port_returns_an_opaque_permit_and_enterprise_denial_cannot_be_widened() {
    let directory = temporary_directory("permit");
    let storage = SqliteStorage::open(&directory).expect("storage");
    let mut admission = DurableEnterpriseQuotaAdmission::new(storage);
    admission
        .put_policy(&EnterpriseQuotaPolicy {
            boundary: EnterpriseQuotaBoundary::Organization {
                organization_id: attribution().organization_id,
            },
            revision: 1,
            limits: EnterpriseQuotaLimits {
                max_concurrent: Some(1),
                ..EnterpriseQuotaLimits::default()
            },
        })
        .expect("policy");
    let first_request = request(1);
    let permit = match admission.reserve(&first_request).expect("first admission") {
        EnterpriseQuotaAdmission::Admitted(permit) => permit,
        EnterpriseQuotaAdmission::TerminalReplay(receipt) => {
            panic!("unexpected terminal replay: {receipt:?}")
        }
        EnterpriseQuotaAdmission::Denied(denial) => panic!("unexpected denial: {denial:?}"),
    };
    assert_eq!(
        permit.receipt().record.reservation_id,
        first_request.reservation_id
    );
    let second_request = request(2);
    let denial = match admission
        .reserve(&second_request)
        .expect("enterprise decision")
    {
        EnterpriseQuotaAdmission::Denied(denial) => denial,
        EnterpriseQuotaAdmission::Admitted(_) => panic!("enterprise denial was widened"),
        EnterpriseQuotaAdmission::TerminalReplay(receipt) => {
            panic!("unexpected terminal replay: {receipt:?}")
        }
    };
    assert_eq!(denial.dimension, EnterpriseQuotaDimension::Concurrent);
    admission.close().expect("close");

    let storage = SqliteStorage::open(&directory).expect("restart storage");
    let mut admission = DurableEnterpriseQuotaAdmission::new(storage);
    let replay = admission.reserve(&first_request).expect("restart replay");
    assert!(matches!(replay, EnterpriseQuotaAdmission::Admitted(_)));
    admission
        .release(&EnterpriseQuotaRelease {
            reservation_id: first_request.reservation_id.clone(),
            request_id: RequestId(id("req", 3)),
            expected_revision: 1,
            reason: EnterpriseQuotaReleaseReason::Cancelled,
            released_at: Instant("2027-05-01T08:00:01.000Z".to_owned()),
        })
        .expect("release");
    assert!(matches!(
        admission.reserve(&first_request).expect("terminal replay"),
        EnterpriseQuotaAdmission::TerminalReplay(_)
    ));
    assert!(matches!(
        admission
            .reserve(&second_request)
            .expect("capacity after release"),
        EnterpriseQuotaAdmission::Admitted(_)
    ));
    admission.close().expect("final close");
    fs::remove_dir_all(directory).expect("cleanup");
}
