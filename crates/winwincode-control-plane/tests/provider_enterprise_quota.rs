// SPDX-License-Identifier: Apache-2.0

use winwincode_control_plane::{
    EnterpriseQuotaAdmission, EnterpriseQuotaAdmissionPort, ModelRetrySettlementContext,
    ModelRetrySettlementContextError, ModelRetrySettlementContextPort,
    ProviderEnterpriseQuotaErrorKind, ProviderEnterpriseQuotaSaga,
    ProviderOperationalAdmissionError, ProviderOperationalAdmissionPort,
};
use winwincode_domain::{Instant, ModelExchangeId};
use winwincode_storage::{
    EnterpriseQuotaAmounts, EnterpriseQuotaError, EnterpriseQuotaRelease,
    EnterpriseQuotaReservationReceipt, EnterpriseQuotaReservationRequest,
    EnterpriseQuotaSettlement, EnterpriseUsageSource,
};

struct MissingContext;

impl ModelRetrySettlementContextPort for MissingContext {
    fn load_context(
        &self,
        _model_exchange_id: &ModelExchangeId,
    ) -> Result<Option<ModelRetrySettlementContext>, ModelRetrySettlementContextError> {
        Ok(None)
    }
}

struct NeverQuota;

impl EnterpriseQuotaAdmissionPort for NeverQuota {
    fn reserve(
        &mut self,
        _request: &EnterpriseQuotaReservationRequest,
    ) -> Result<EnterpriseQuotaAdmission, EnterpriseQuotaError> {
        panic!("quota must not run without durable context")
    }

    fn release(
        &mut self,
        _release: &EnterpriseQuotaRelease,
    ) -> Result<EnterpriseQuotaReservationReceipt, EnterpriseQuotaError> {
        panic!("quota release must not run without durable context")
    }

    fn settle(
        &mut self,
        _settlement: &EnterpriseQuotaSettlement,
    ) -> Result<EnterpriseQuotaReservationReceipt, EnterpriseQuotaError> {
        panic!("quota settlement must not run without durable context")
    }

    fn settle_usage_source(
        &mut self,
        _source: &EnterpriseUsageSource,
    ) -> Result<Option<EnterpriseQuotaReservationReceipt>, EnterpriseQuotaError> {
        panic!("quota recovery settlement must not run without durable context")
    }
}

struct NeverOperational;

impl ProviderOperationalAdmissionPort for NeverOperational {
    type Receipt = ();

    fn reserve(&mut self) -> Result<Self::Receipt, ProviderOperationalAdmissionError> {
        panic!("operational admission must not run without durable context")
    }
}

#[test]
fn provider_quota_requires_durable_context_before_any_admission() {
    let mut quota = NeverQuota;
    let mut operational = NeverOperational;
    let error = ProviderEnterpriseQuotaSaga::new(&mut quota)
        .reserve_then_admit(
            &MissingContext,
            &ModelExchangeId("mdl_00000000000000000000000001".to_owned()),
            EnterpriseQuotaAmounts {
                operations: 1,
                ..EnterpriseQuotaAmounts::default()
            },
            Instant("2027-08-01T00:00:00.000Z".to_owned()),
            &mut operational,
        )
        .expect_err("missing context must fail closed");
    assert_eq!(
        error.kind(),
        ProviderEnterpriseQuotaErrorKind::ContextMissing
    );
}
