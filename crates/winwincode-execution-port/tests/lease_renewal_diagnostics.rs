// SPDX-License-Identifier: Apache-2.0

use winwincode_domain::Instant;
use winwincode_execution_port::execution_identity::{lease_renewal_rejection, valid_lease_renewal};
use winwincode_execution_port::generated::{ExecutionLeaseStamp, LeaseRenewMessage};

fn fixture() -> (ExecutionLeaseStamp, LeaseRenewMessage, Instant) {
    let current: ExecutionLeaseStamp = serde_json::from_value(serde_json::json!({
        "attempt": 1,
        "expiresAt": "2026-10-09T00:15:00.000Z",
        "fencingToken": "1",
        "issuedAt": "2026-10-09T00:00:00.000Z",
        "jobId": "job_00000000000000000000000001",
        "leaseId": "lse_00000000000000000000000001",
        "workerId": "wrk_00000000000000000000000001",
        "workerInstanceId": "wki_00000000000000000000000001"
    }))
    .unwrap();
    let mut next = current.clone();
    next.expires_at = Instant("2026-10-09T00:28:00.000Z".into());
    let renewal: LeaseRenewMessage = serde_json::from_value(serde_json::json!({
        "kind": "lease.renew", "lease": next,
        "messageId": "xmsg_00000000000000000000000001",
        "priorExpiresAt": current.expires_at,
        "requestId": "req_00000000000000000000000001",
        "schemaVersion": "winwincode/v1",
        "sentAt": "2026-10-09T00:13:00.000Z"
    }))
    .unwrap();
    (current, renewal, Instant("2026-10-09T00:13:01.000Z".into()))
}

fn assert_rejected(
    current: &ExecutionLeaseStamp,
    renewal: &LeaseRenewMessage,
    now: &Instant,
    code: &str,
) {
    let original = serde_json::to_vec(&(current, renewal)).unwrap();
    assert_eq!(lease_renewal_rejection(current, renewal, now), Some(code));
    assert!(!valid_lease_renewal(current, renewal, now));
    assert_eq!(serde_json::to_vec(&(current, renewal)).unwrap(), original);
}

#[test]
fn same_attempt_extension_is_accepted_without_rewriting_the_frame() {
    let (current, renewal, now) = fixture();
    let original = serde_json::to_vec(&renewal).unwrap();
    assert_eq!(lease_renewal_rejection(&current, &renewal, &now), None);
    assert!(valid_lease_renewal(&current, &renewal, &now));
    assert_eq!(serde_json::to_vec(&renewal).unwrap(), original);
}

#[test]
fn exact_accepted_renewal_replay_is_accepted() {
    let (_, renewal, now) = fixture();
    assert_eq!(
        lease_renewal_rejection(&renewal.lease, &renewal, &now),
        None
    );
    assert!(valid_lease_renewal(&renewal.lease, &renewal, &now));
}

#[test]
fn noncanonical_time_is_rejected() {
    let (current, renewal, _) = fixture();
    assert_rejected(
        &current,
        &renewal,
        &Instant("2026-10-09T00:13:01Z".into()),
        "noncanonical_time",
    );
}

#[test]
fn every_authority_field_remains_strict() {
    let (current, renewal, now) = fixture();
    for (field, value) in [
        ("attempt", serde_json::json!(2)),
        ("fencingToken", serde_json::json!("2")),
        ("issuedAt", serde_json::json!("2026-10-09T00:00:00.001Z")),
        ("jobId", serde_json::json!("job_00000000000000000000000002")),
        (
            "leaseId",
            serde_json::json!("lse_00000000000000000000000002"),
        ),
        (
            "workerId",
            serde_json::json!("wrk_00000000000000000000000002"),
        ),
        (
            "workerInstanceId",
            serde_json::json!("wki_00000000000000000000000002"),
        ),
    ] {
        let mut changed = serde_json::to_value(&renewal).unwrap();
        changed["lease"][field] = value;
        let changed = serde_json::from_value(changed).unwrap();
        assert_rejected(&current, &changed, &now, "authority_mismatch");
    }
}

#[test]
fn sent_before_issued_is_rejected() {
    let (current, mut renewal, now) = fixture();
    renewal.sent_at = Instant("2026-10-08T23:59:59.999Z".into());
    assert_rejected(&current, &renewal, &now, "sent_before_issued");
}

#[test]
fn future_send_is_rejected() {
    let (current, mut renewal, now) = fixture();
    renewal.sent_at = Instant("2026-10-09T00:13:02.000Z".into());
    assert_rejected(&current, &renewal, &now, "sent_in_future");
}

#[test]
fn send_at_original_deadline_is_rejected() {
    let (current, mut renewal, _) = fixture();
    renewal.sent_at = renewal.prior_expires_at.clone();
    assert_rejected(
        &current,
        &renewal,
        &renewal.sent_at,
        "sent_after_prior_expiry",
    );
}

#[test]
fn nonextending_expiry_is_rejected() {
    let (current, mut renewal, now) = fixture();
    renewal.lease.expires_at = renewal.prior_expires_at.clone();
    assert_rejected(&current, &renewal, &now, "nonextending_expiry");
}

#[test]
fn consumption_at_original_deadline_is_rejected_even_if_sent_in_time() {
    let (current, renewal, _) = fixture();
    assert_rejected(
        &current,
        &renewal,
        &current.expires_at,
        "consumed_after_expiry",
    );
}

#[test]
fn unrelated_prior_expiry_is_rejected() {
    let (current, mut renewal, now) = fixture();
    renewal.prior_expires_at = Instant("2026-10-09T00:16:00.000Z".into());
    assert_rejected(&current, &renewal, &now, "prior_expiry_mismatch");
}
