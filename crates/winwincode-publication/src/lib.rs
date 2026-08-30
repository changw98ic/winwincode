// SPDX-License-Identifier: Apache-2.0

//! Publication domain and durable external-effect coordinator.

mod coordinator;
mod facts;
mod github;
mod metering;
mod operation;
mod policy;

pub use coordinator::{
    Publication, PublicationCancelCommand, PublicationCommandContext, PublicationCoordinator,
    PublicationError, PublicationErrorKind, PublicationLedger, PublicationPublishCommand,
    PublicationReadLedger, PublicationState,
};
pub use facts::{
    PublicationAuthorization, PublicationFactBinding, PublicationResourceFact,
    PublicationResourceKind, PublicationResultFact, PublicationSourceIssue, PublicationTarget,
};
pub use github::{
    CredentialResolutionError, GitHubAdapterConfig, GitHubCredential, GitHubCredentialResolver,
    GitHubPublicationAdapter,
};
pub use metering::{
    PublicationEnterpriseAttribution, PublicationMeteringCursor, PublicationMeteringError,
    PublicationMeteringErrorKind, PublicationMeteringFilter, PublicationMeteringLedger,
    PublicationMeteringSourceEntry, PublicationMeteringSourcePage,
};
pub use operation::{
    PUBLICATION_OPERATION_PROTOCOL, PUBLICATION_OPERATION_SCHEMA_VERSION, PublicationOperation,
    PublicationOperationKind, PublicationOperationPayload, PublicationPort, PublicationPortError,
    PublicationPortMutation, PublicationPortObservation,
};
pub use policy::{
    PolicyPermission, PublicationPolicyAudit, PublicationPolicyAuditError,
    PublicationPolicyContext, PublicationPolicyDecision, PublicationPolicyEffect,
    PublicationPolicyEvidence, PublicationPolicyOrigin, PublicationPolicyRule,
    PublicationRequester, RepositoryPolicyScope, RepositoryPublicationPolicy,
};

#[cfg(feature = "test-support")]
pub mod test_support;
