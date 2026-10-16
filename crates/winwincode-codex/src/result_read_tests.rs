// SPDX-License-Identifier: Apache-2.0
use super::super::ExecutionPortActionGate;
use super::super::tests::{binding, catalog};
use winwincode_domain::Instant;

use winwincode_kernel::{KernelActionGate, KernelToolResultReadRequest};

fn read_request() -> KernelToolResultReadRequest {
    KernelToolResultReadRequest {
        session_id: "kernel-session-action".into(),
        turn_id: "current-turn".into(),
        operation_id: "current-call".into(),
        request_sequence: 12,
        logical_id: "original-request".into(),
        attempt_id: "original-attempt".into(),
        operation_digest: "a".repeat(64),
        revision: 4,
    }
}

#[tokio::test]
async fn stored_result_read_is_receipt_bound_and_current_lease_fenced() {
    let home = std::env::temp_dir().join(format!("wwc-result-read-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&home).unwrap();
    let gate = ExecutionPortActionGate::open(
        home.as_path(),
        catalog(),
        crate::model_bridge::SharedAuthoritySource::default(),
    )
    .unwrap();
    gate.install_binding(&binding(), None).unwrap();
    gate.update_now(&Instant("2030-01-01T00:00:02.000Z".into()))
        .unwrap();
    let request = read_request();
    let authorization = gate.authorize_result_read(request.clone()).await.unwrap();
    gate.revalidate_result_read(request.clone(), authorization.clone())
        .await
        .unwrap();
    let mut different = request.clone();
    different.request_sequence += 1;
    assert!(
        gate.revalidate_result_read(different, authorization.clone())
            .await
            .is_err()
    );
    gate.cancel_session(&request.session_id).unwrap();
    assert!(
        gate.revalidate_result_read(request, authorization)
            .await
            .is_err()
    );
    let _ = std::fs::remove_dir_all(&home);
}

#[tokio::test]
async fn expired_unknown_and_delegated_result_reads_are_rejected() {
    let home = std::env::temp_dir().join(format!("wwc-result-read-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&home).unwrap();
    let gate = ExecutionPortActionGate::open(
        home.as_path(),
        catalog(),
        crate::model_bridge::SharedAuthoritySource::default(),
    )
    .unwrap();
    let request = read_request();
    assert!(gate.authorize_result_read(request.clone()).await.is_err());
    gate.install_binding(&binding(), Some(home.as_path()))
        .unwrap();
    gate.update_now(&Instant("2030-01-01T00:00:02.000Z".into()))
        .unwrap();
    assert!(gate.authorize_result_read(request.clone()).await.is_err());
    gate.install_binding(&binding(), None).unwrap();
    gate.update_now(&Instant("2030-01-01T01:00:00.000Z".into()))
        .unwrap();
    assert!(gate.authorize_result_read(request).await.is_err());
    let _ = std::fs::remove_dir_all(&home);
}

#[tokio::test]
async fn result_read_authority_expires_without_owner_clock_refresh() {
    let home =
        std::env::temp_dir().join(format!("wwc-result-read-elapsed-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&home).unwrap();
    let source = crate::model_bridge::SharedAuthoritySource::default();
    let gate = ExecutionPortActionGate::open(&home, catalog(), source.clone()).unwrap();
    let authority = binding();
    gate.install_binding(&authority, None).unwrap();
    let initial = Instant("2030-01-01T00:00:02.000Z".into());
    gate.update_now(&initial).unwrap();
    let request = read_request();
    let authorization = gate.authorize_result_read(request.clone()).await.unwrap();
    gate.revalidate_result_read(request.clone(), authorization.clone())
        .await
        .unwrap();
    source.advance_clock_for_test(std::time::Duration::from_hours(1));
    assert!(gate.authorize_result_read(request.clone()).await.is_err());
    assert!(
        gate.revalidate_result_read(request.clone(), authorization.clone())
            .await
            .is_err()
    );
    gate.update_now(&initial).unwrap();
    assert!(gate.authorize_result_read(request.clone()).await.is_err());
    assert!(
        gate.revalidate_result_read(request.clone(), authorization)
            .await
            .is_err()
    );
    gate.cancel_session(&request.session_id).unwrap();
    gate.remove_binding(&authority).unwrap();
    std::fs::remove_dir_all(home).unwrap();
}
