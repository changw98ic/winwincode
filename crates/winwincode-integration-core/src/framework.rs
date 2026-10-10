use crate::{
    ConnectorAuthority, ConnectorCallErrorKind, ConnectorPort, ConnectorRegistration,
    ConnectorRegistrationReceipt, InboundNormalizationContext, InboundReceipt,
    InboundWebhookRequest, IntegrationError, IntegrationErrorKind, IntegrationId,
    IntegrationLeaseId, IntegrationOperationKey, IntegrationScope, IntegrationStoragePort,
    OutboundAttemptResult, OutboundEnqueueReceipt, OutboundOperation, OutboundRequest,
    SignatureVerificationErrorKind, WebhookSignatureVerifier,
};

pub struct IntegrationFramework {
    storage: Box<dyn IntegrationStoragePort>,
}

impl IntegrationFramework {
    #[must_use]
    pub fn new(storage: impl IntegrationStoragePort + 'static) -> Self {
        Self {
            storage: Box::new(storage),
        }
    }

    #[must_use]
    pub fn storage(&self) -> &dyn IntegrationStoragePort {
        self.storage.as_ref()
    }

    #[must_use]
    pub fn storage_mut(&mut self) -> &mut dyn IntegrationStoragePort {
        self.storage.as_mut()
    }

    pub fn register_connector(
        &mut self,
        registration: &ConnectorRegistration,
    ) -> Result<ConnectorRegistrationReceipt, IntegrationError> {
        self.storage.register(registration)
    }

    pub fn revoke_credential(
        &mut self,
        scope: &IntegrationScope,
        integration_id: &IntegrationId,
        expected_revision: u64,
        occurred_at_millis: u64,
    ) -> Result<ConnectorAuthority, IntegrationError> {
        self.storage
            .revoke_credential(scope, integration_id, expected_revision, occurred_at_millis)
    }

    pub fn receive_webhook(
        &mut self,
        request: &InboundWebhookRequest,
        verifier: &mut dyn WebhookSignatureVerifier,
        connector: &mut dyn ConnectorPort,
    ) -> Result<InboundReceipt, IntegrationError> {
        let authority = self
            .storage
            .authority(request.scope(), request.integration_id())?;
        if authority.state() != crate::ConnectorState::Active {
            return Err(IntegrationError::new(
                IntegrationErrorKind::CredentialRevoked,
                "connector credential is revoked",
            ));
        }
        if let Err(error) = verifier.verify(&authority, request.signature(), request.payload()) {
            return match error.kind() {
                SignatureVerificationErrorKind::Rejected => Err(IntegrationError::new(
                    IntegrationErrorKind::SignatureRejected,
                    "webhook signature was rejected",
                )),
                SignatureVerificationErrorKind::CredentialRevoked => {
                    self.storage.revoke_credential(
                        request.scope(),
                        request.integration_id(),
                        authority.revision(),
                        request.received_at_millis(),
                    )?;
                    Err(IntegrationError::new(
                        IntegrationErrorKind::CredentialRevoked,
                        "connector credential is revoked",
                    ))
                }
            };
        }
        let context = InboundNormalizationContext::from_request(request);
        let normalized = connector
            .normalize_inbound(&authority, &context, request.payload())
            .map_err(|error| map_connector_error(&error))?;
        self.storage
            .accept_inbound(&authority, request, &normalized)
    }

    pub fn enqueue_outbound(
        &mut self,
        request: &OutboundRequest,
    ) -> Result<OutboundEnqueueReceipt, IntegrationError> {
        self.storage.enqueue_outbound(request)
    }

    pub fn deliver_next(
        &mut self,
        scope: &IntegrationScope,
        integration_id: &IntegrationId,
        now_millis: u64,
        lease_id: IntegrationLeaseId,
        lease_expires_at_millis: u64,
        connector: &mut dyn ConnectorPort,
    ) -> Result<Option<OutboundAttemptResult>, IntegrationError> {
        let Some(claim) = self.storage.claim_due(
            scope,
            integration_id,
            now_millis,
            lease_id,
            lease_expires_at_millis,
        )?
        else {
            return Ok(None);
        };
        let result = match connector.deliver_outbound(&claim) {
            Ok(remote) => OutboundAttemptResult::Delivered(
                self.storage
                    .record_success(scope, &claim, &remote, now_millis)?,
            ),
            Err(failure) => self
                .storage
                .record_failure(scope, &claim, &failure, now_millis)?,
        };
        Ok(Some(result))
    }

    pub fn outbound_operation(
        &self,
        scope: &IntegrationScope,
        integration_id: &IntegrationId,
        operation_key: &IntegrationOperationKey,
    ) -> Result<OutboundOperation, IntegrationError> {
        self.storage
            .outbound_operation(scope, integration_id, operation_key)
    }
}

fn map_connector_error(error: &crate::ConnectorCallError) -> IntegrationError {
    match error.kind() {
        ConnectorCallErrorKind::CredentialRevoked => IntegrationError::new(
            IntegrationErrorKind::CredentialRevoked,
            "connector credential is revoked",
        ),
        ConnectorCallErrorKind::Retryable | ConnectorCallErrorKind::Permanent => {
            IntegrationError::new(
                IntegrationErrorKind::ConnectorRejected,
                "connector rejected the inbound payload",
            )
        }
    }
}
