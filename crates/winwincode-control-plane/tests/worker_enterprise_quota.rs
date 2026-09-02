// SPDX-License-Identifier: Apache-2.0

use winwincode_control_plane::{
    EnterpriseQuotaAdmission, EnterpriseQuotaAdmissionPort, WorkerEnterpriseQuotaAuthority,
    WorkerEnterpriseQuotaAuthorityPort, WorkerEnterpriseQuotaError, WorkerEnterpriseQuotaErrorKind,
    WorkerEnterpriseQuotaSaga, WorkerOperationalClaimPort,
};
use winwincode_domain::{ExecutionJobId, Instant};
use winwincode_storage::{
    EnterpriseQuotaError, EnterpriseQuotaRelease, EnterpriseQuotaReservationReceipt,
    EnterpriseQuotaReservationRequest, EnterpriseQuotaSettlement, EnterpriseUsageSource,
    ExecutionLeaseClaim,
};

struct MissingAuthority;

impl WorkerEnterpriseQuotaAuthorityPort for MissingAuthority {
    fn load(
        &mut self,
        _job_id: &ExecutionJobId,
        _claim: &ExecutionLeaseClaim,
    ) -> Result<Option<WorkerEnterpriseQuotaAuthority>, WorkerEnterpriseQuotaError> {
        Ok(None)
    }
}

struct NeverQuota;

impl EnterpriseQuotaAdmissionPort for NeverQuota {
    fn reserve(
        &mut self,
        _request: &EnterpriseQuotaReservationRequest,
    ) -> Result<EnterpriseQuotaAdmission, EnterpriseQuotaError> {
        panic!("quota must not run without durable Worker authority")
    }

    fn release(
        &mut self,
        _release: &EnterpriseQuotaRelease,
    ) -> Result<EnterpriseQuotaReservationReceipt, EnterpriseQuotaError> {
        panic!("quota release must not run without durable Worker authority")
    }

    fn settle(
        &mut self,
        _settlement: &EnterpriseQuotaSettlement,
    ) -> Result<EnterpriseQuotaReservationReceipt, EnterpriseQuotaError> {
        panic!("quota settlement must not run without durable Worker authority")
    }

    fn settle_usage_source(
        &mut self,
        _source: &EnterpriseUsageSource,
    ) -> Result<Option<EnterpriseQuotaReservationReceipt>, EnterpriseQuotaError> {
        panic!("quota recovery must not run without durable Worker authority")
    }
}

struct NeverClaim;

impl WorkerOperationalClaimPort for NeverClaim {
    type Receipt = ();

    fn claim(
        &mut self,
        _authority: &WorkerEnterpriseQuotaAuthority,
    ) -> Result<Self::Receipt, WorkerEnterpriseQuotaError> {
        panic!("operational claim must not run without durable Worker authority")
    }
}

#[test]
fn worker_quota_requires_durable_job_and_authenticated_placement_before_any_admission() {
    let mut quota = NeverQuota;
    let mut claim = NeverClaim;
    let mut authorities = MissingAuthority;
    let error = WorkerEnterpriseQuotaSaga::new(&mut quota)
        .reserve_then_claim(
            &mut authorities,
            &ExecutionJobId("job_00000000000000000000000001".to_owned()),
            &ExecutionLeaseClaim {
                expires_at: Instant("2027-08-01T00:01:00.000Z".to_owned()),
                fencing_token: winwincode_domain::FencingToken("1".to_owned()),
                issued_at: Instant("2027-08-01T00:00:00.000Z".to_owned()),
                job_id: ExecutionJobId("job_00000000000000000000000001".to_owned()),
                lease_id: winwincode_domain::LeaseId("lse_00000000000000000000000001".to_owned()),
                message_id: winwincode_domain::ExecutionMessageId(
                    "xmsg_00000000000000000000000001".to_owned(),
                ),
                payload_digest: winwincode_domain::Sha256Digest(format!(
                    "sha256:{}",
                    "1".repeat(64)
                )),
                request_id: winwincode_domain::RequestId(
                    "req_00000000000000000000000001".to_owned(),
                ),
                worker_id: winwincode_domain::WorkerId("wrk_00000000000000000000000001".to_owned()),
                worker_instance_id: winwincode_domain::WorkerInstanceId(
                    "wki_00000000000000000000000001".to_owned(),
                ),
                attempt: 1,
            },
            Instant("2027-08-01T00:00:00.000Z".to_owned()),
            &mut claim,
        )
        .expect_err("missing durable authority must fail closed");
    assert_eq!(
        error.kind(),
        WorkerEnterpriseQuotaErrorKind::AuthorityMissing
    );
}
