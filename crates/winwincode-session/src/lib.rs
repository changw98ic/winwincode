// SPDX-License-Identifier: Apache-2.0

//! Control Plane `ProductSession` lifecycle and exact execution identity seam.
//!
//! This crate deliberately contains no Delivery, API DTO, Worker, Codex Core,
//! or Control Plane composition dependency. The migration implementation is
//! kept in [`migration`] so legacy conversion stays a separate vertical slice.

mod binding;
mod interaction_routing;
mod product_session;
mod sqlite_migration;

#[allow(clippy::missing_errors_doc)]
pub mod migration;

pub use binding::{
    BindingScope, RuntimeSourceIdentity, SessionBinding, SessionBindingError,
    SessionBindingIdentity,
};
pub use interaction_routing::{
    AuthenticatedActor, DecisionRouteBinding, ExecutionCancellationRoutes, ExecutionRoute,
    InteractionDecision, InteractionExpiry, InteractionOutcome, InteractionRegistration,
    InteractionResponse, InteractionRouteReceipt, InteractionRouter, InteractionRoutingError,
    InteractionSubject, JobCancellationRoute, ModelStreamCancellationRoute, RouteWriteStatus,
    RuntimeRouteAuthority, SessionCancellationReceipt, SessionCancellationRequest,
    SessionCancellationSnapshot, WorkerCancellationRoute,
};
pub use product_session::{
    ProductSession, ProductSessionCreate, ProductSessionError, ProductSessionState,
};
pub use sqlite_migration::{SqliteSessionIdentityMigration, SqliteSessionIdentityMigrationError};
