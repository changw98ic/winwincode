// SPDX-License-Identifier: Apache-2.0

//! Control Plane `ProductSession` lifecycle and exact execution identity seam.
//!
//! This crate deliberately contains no Delivery, API DTO, Worker, Codex Core,
//! storage adapter, or Control Plane composition dependency.

mod binding;
mod interaction_routing;
mod product_session;

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
