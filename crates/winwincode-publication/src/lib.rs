// SPDX-License-Identifier: Apache-2.0

//! Publication domain and durable external-effect coordinator.

mod coordinator;
mod facts;
mod github;
mod operation;
mod policy;
mod storage;

pub use coordinator::{
    MAX_PUBLICATION_DETAIL_HISTORY, Publication, PublicationCancelCommand,
    PublicationCommandContext, PublicationCoordinator, PublicationDetail, PublicationError,
    PublicationErrorKind, PublicationLedger, PublicationPublishCommand, PublicationReadLedger,
    PublicationState, PublicationStatusHistory, PublicationStepDetail, PublicationStepState,
};
pub use facts::{
    PublicationAuthorization, PublicationFactBinding, PublicationResourceFact,
    PublicationResourceKind, PublicationResultFact, PublicationSourceIssue, PublicationTarget,
};
pub use github::{
    CredentialResolutionError, GitHubAdapterConfig, GitHubCredential, GitHubCredentialResolver,
    GitHubPublicationAdapter,
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
pub use storage::{
    PublicationJournalMutation, PublicationJournalRecord, PublicationReceiptIdentity,
    PublicationStorage, PublicationStorageCommit, PublicationStorageError,
    PublicationStorageErrorKind, PublicationStorageEvent, PublicationStorageReceipt,
    PublicationStoredJournal, PublicationStoredState,
};

#[cfg(feature = "test-support")]
pub mod test_support;
