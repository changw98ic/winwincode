// SPDX-License-Identifier: Apache-2.0

//! Enterprise quota admission at the Publication provider boundary.
//!
//! [`PublicationEnterpriseQuotaSaga`] is a [`PublicationPort`] decorator. It
//! reserves one enterprise operation from the immutable Publication
//! attribution before every provider lookup or write. Terminal no-write
//! outcomes release that reservation; uncertain outcomes deliberately leave it
//! active. A confirmed existing or remote-written operation is settled later
//! only by [`crate::PublicationEnterpriseUsageReconciler`], after the
//! Publication coordinator has atomically written its immutable source.

use sha2::{Digest, Sha256};
use winwincode_domain::{Instant, PublicationId, RequestId};
use winwincode_publication::{
    PublicationEnterpriseAttribution, PublicationOperation, PublicationPort, PublicationPortError,
    PublicationPortMutation, PublicationPortObservation,
};
use winwincode_storage::{
    EnterpriseQuotaAmounts, EnterpriseQuotaRelease, EnterpriseQuotaReleaseReason,
    EnterpriseQuotaReservationRequest, EnterpriseQuotaSourceSeal, EnterpriseUsageAttribution,
};

use crate::{
    EnterpriseQuotaAdmission, EnterpriseQuotaAdmissionPort, EnterpriseQuotaPermit, StorageError,
    instant_from_millis,
};

const REQUEST_PREFIX: &str = "req_";
const CROCKFORD_BASE32: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
const QUOTA_DENIED_CODE: &str = "enterprise-quota-denied";
const QUOTA_UNAVAILABLE_CODE: &str = "enterprise-quota-unavailable";
const QUOTA_TERMINAL_CODE: &str = "enterprise-quota-terminal-replay";

/// Publication provider decorator that preserves the existing provider as the
/// only provider authority while placing enterprise allowance before it.
///
/// `requested_at` is a durable operation timestamp supplied by the caller. It
/// must remain identical for a restart/replay of the same Publication work;
/// this is part of the quota reservation identity.
pub struct PublicationEnterpriseQuotaSaga<'quota, 'provider> {
    quota: &'quota mut dyn EnterpriseQuotaAdmissionPort,
    provider: &'provider mut dyn PublicationPort,
    attribution: PublicationEnterpriseAttribution,
    publication_id: PublicationId,
    requested_at: Instant,
}

/// Converts the immutable Publication approval fact into the one timestamp
/// used by every enterprise-quota replay for that Publication.
pub(crate) fn publication_quota_requested_at(
    approved_at_millis: u64,
) -> Result<Instant, StorageError> {
    instant_from_millis(approved_at_millis)
}

impl<'quota, 'provider> PublicationEnterpriseQuotaSaga<'quota, 'provider> {
    /// Wraps one configured provider with the unique enterprise quota port.
    #[must_use]
    pub fn new(
        quota: &'quota mut dyn EnterpriseQuotaAdmissionPort,
        provider: &'provider mut dyn PublicationPort,
        attribution: &PublicationEnterpriseAttribution,
        publication_id: &PublicationId,
        requested_at: Instant,
    ) -> Self {
        Self {
            quota,
            provider,
            attribution: attribution.clone(),
            publication_id: publication_id.clone(),
            requested_at,
        }
    }

    /// Returns the exact deterministic reserve request for one provider operation.
    #[must_use]
    pub fn reservation_request(
        &self,
        operation: &PublicationOperation,
    ) -> EnterpriseQuotaReservationRequest {
        EnterpriseQuotaReservationRequest {
            reservation_id: quota_request_id(
                "reserve",
                &self.attribution,
                &self.publication_id,
                operation.operation_key(),
                operation.request_sha256(),
            ),
            attribution: usage_attribution(&self.attribution),
            source_seal: EnterpriseQuotaSourceSeal::Publication {
                publication_id: self.publication_id.clone(),
                operation_key: operation.operation_key().to_owned(),
                request_sha256: operation.request_sha256().to_owned(),
            },
            reserved: EnterpriseQuotaAmounts {
                operations: 1,
                ..EnterpriseQuotaAmounts::default()
            },
            requested_at: self.requested_at.clone(),
        }
    }

    fn reserve(&mut self, operation: &PublicationOperation) -> ReserveOutcome {
        let request = self.reservation_request(operation);
        match self.quota.reserve(&request) {
            Ok(EnterpriseQuotaAdmission::Admitted(permit)) => {
                ReserveOutcome::Active(Box::new(ActiveReservation {
                    request,
                    permit: *permit,
                }))
            }
            Ok(EnterpriseQuotaAdmission::TerminalReplay(_)) => ReserveOutcome::Terminal,
            Ok(EnterpriseQuotaAdmission::Denied(_)) => ReserveOutcome::Denied,
            Err(_) => ReserveOutcome::Unavailable,
        }
    }

    fn release(
        &mut self,
        request: &EnterpriseQuotaReservationRequest,
        permit: &EnterpriseQuotaPermit,
        reason: EnterpriseQuotaReleaseReason,
    ) -> bool {
        self.quota
            .release(&EnterpriseQuotaRelease {
                reservation_id: request.reservation_id.clone(),
                request_id: quota_request_id(
                    release_action(reason),
                    &self.attribution,
                    &self.publication_id,
                    publication_operation_key(request),
                    publication_request_sha256(request),
                ),
                expected_revision: permit.receipt().record.revision,
                reason,
                released_at: self.requested_at.clone(),
            })
            .is_ok()
    }
}

impl PublicationPort for PublicationEnterpriseQuotaSaga<'_, '_> {
    fn lookup(
        &mut self,
        operation: &PublicationOperation,
    ) -> Result<PublicationPortObservation, PublicationPortError> {
        let admission = self.reserve(operation);
        let ReserveOutcome::Active(active) = &admission else {
            return Ok(observation_for_reserve(operation, &admission));
        };
        let observation = self.provider.lookup(operation)?;
        if matches!(observation, PublicationPortObservation::Conflict { .. })
            && !self.release(
                &active.request,
                &active.permit,
                EnterpriseQuotaReleaseReason::Failed,
            )
        {
            return Ok(PublicationPortObservation::unknown(
                operation,
                QUOTA_UNAVAILABLE_CODE,
            ));
        }
        Ok(observation)
    }

    fn apply(
        &mut self,
        operation: &PublicationOperation,
    ) -> Result<PublicationPortMutation, PublicationPortError> {
        let admission = self.reserve(operation);
        let ReserveOutcome::Active(active) = &admission else {
            return Ok(mutation_for_reserve(operation, &admission));
        };
        let mutation = self.provider.apply(operation)?;
        let release_reason = match mutation {
            PublicationPortMutation::Applied {
                remote_write_performed: false,
                ..
            } => Some(EnterpriseQuotaReleaseReason::Cancelled),
            PublicationPortMutation::Rejected { .. } => Some(EnterpriseQuotaReleaseReason::Failed),
            PublicationPortMutation::Applied {
                remote_write_performed: true,
                ..
            }
            | PublicationPortMutation::Unknown { .. } => None,
        };
        match release_reason {
            Some(reason) if !self.release(&active.request, &active.permit, reason) => {
                return Ok(PublicationPortMutation::unknown(
                    operation,
                    QUOTA_UNAVAILABLE_CODE,
                ));
            }
            Some(_) | None => {}
        }
        Ok(mutation)
    }
}

enum ReserveOutcome {
    Active(Box<ActiveReservation>),
    Terminal,
    Denied,
    Unavailable,
}

struct ActiveReservation {
    request: EnterpriseQuotaReservationRequest,
    permit: EnterpriseQuotaPermit,
}

fn observation_for_reserve(
    operation: &PublicationOperation,
    outcome: &ReserveOutcome,
) -> PublicationPortObservation {
    match outcome {
        ReserveOutcome::Denied => {
            PublicationPortObservation::conflict(operation, QUOTA_DENIED_CODE)
        }
        ReserveOutcome::Terminal => {
            PublicationPortObservation::unknown(operation, QUOTA_TERMINAL_CODE)
        }
        ReserveOutcome::Unavailable | ReserveOutcome::Active(_) => {
            PublicationPortObservation::unknown(operation, QUOTA_UNAVAILABLE_CODE)
        }
    }
}

fn mutation_for_reserve(
    operation: &PublicationOperation,
    outcome: &ReserveOutcome,
) -> PublicationPortMutation {
    match outcome {
        ReserveOutcome::Denied => PublicationPortMutation::rejected(operation, QUOTA_DENIED_CODE),
        ReserveOutcome::Terminal => {
            PublicationPortMutation::unknown(operation, QUOTA_TERMINAL_CODE)
        }
        ReserveOutcome::Unavailable | ReserveOutcome::Active(_) => {
            PublicationPortMutation::unknown(operation, QUOTA_UNAVAILABLE_CODE)
        }
    }
}

fn usage_attribution(source: &PublicationEnterpriseAttribution) -> EnterpriseUsageAttribution {
    EnterpriseUsageAttribution {
        organization_id: source.organization_id().clone(),
        workspace_id: source.workspace_id().clone(),
        project_id: source.project_id().clone(),
        repository_id: source.repository_id().clone(),
        delivery_id: Some(source.delivery_id().clone()),
        product_session_id: Some(source.product_session_id().clone()),
        user_id: source.user_id().clone(),
    }
}

fn release_action(reason: EnterpriseQuotaReleaseReason) -> &'static str {
    match reason {
        EnterpriseQuotaReleaseReason::Cancelled => "release-cancelled",
        EnterpriseQuotaReleaseReason::Failed => "release-failed",
        EnterpriseQuotaReleaseReason::OperationalAdmissionDenied => "release-operational-denied",
    }
}

fn quota_request_id(
    action: &str,
    attribution: &PublicationEnterpriseAttribution,
    publication_id: &PublicationId,
    operation_key: &str,
    request_sha256: &str,
) -> RequestId {
    let mut hasher = Sha256::new();
    for value in [
        action,
        &attribution.organization_id().0,
        &attribution.workspace_id().0,
        &attribution.project_id().0,
        &attribution.repository_id().0,
        &attribution.delivery_id().0,
        &attribution.product_session_id().0,
        &attribution.user_id().0,
        &publication_id.0,
        operation_key,
        request_sha256,
    ] {
        hasher.update(value.as_bytes());
        hasher.update([0]);
    }
    let digest = hasher.finalize();
    let suffix = digest
        .iter()
        .take(26)
        .map(|byte| char::from(CROCKFORD_BASE32[usize::from(byte & 31)]))
        .collect::<String>();
    RequestId(format!("{REQUEST_PREFIX}{suffix}"))
}

fn publication_operation_key(request: &EnterpriseQuotaReservationRequest) -> &str {
    let EnterpriseQuotaSourceSeal::Publication { operation_key, .. } = &request.source_seal else {
        unreachable!("Publication quota saga always creates Publication source seals");
    };
    operation_key
}

fn publication_request_sha256(request: &EnterpriseQuotaReservationRequest) -> &str {
    let EnterpriseQuotaSourceSeal::Publication { request_sha256, .. } = &request.source_seal else {
        unreachable!("Publication quota saga always creates Publication source seals");
    };
    request_sha256
}
