// SPDX-License-Identifier: Apache-2.0

//! WinWinCode Fusion blind independent-reasoning panel.
//!
//! Phase 1 of the Model Dynasty track (community.5.1). Fusion is **not** a
//! multi-agent role split. One canonical question and context are sent to
//! several Providers in parallel; each Provider answers in isolation without
//! seeing sibling outputs. Successful answers surface as independently
//! auditable [`FusionCandidate`] rows. A Provider timeout or failure becomes
//! one failure row and never cancels the rest of the panel.
//!
//! This crate is intentionally separate from the embedded Kernel and from the
//! Jev closed-set adapter. Hosts inject Provider adapters through
//! [`FusionProvider`]; tests and production adapters share the same boundary.
//! Fusion does not define a second execution kernel and does not import
//! Kernel or Codex types.

#![allow(clippy::doc_markdown)]

mod contract;
mod panel;
mod provider;

pub use contract::FusionBlindPrompt;
pub use contract::FusionBudget;
pub use contract::FusionCandidate;
pub use contract::FusionCandidateAudit;
pub use contract::FusionCandidateFailure;
pub use contract::FusionInput;
pub use contract::FusionPanelResult;
pub use contract::FusionProviderAnswer;
pub use contract::FusionProviderCandidate;
pub use contract::FusionProviderRequest;
pub use contract::FusionTokenUsage;
pub use panel::FusionPanelError;
pub use panel::run_blind_panel;
pub use provider::FusionProvider;
pub use provider::FusionProviderError;
pub use provider::FusionProviderRouter;
pub use provider::MapFusionProviderRouter;

/// Minimum distinct Providers required by a valid blind panel.
pub const MIN_PROVIDER_COUNT: usize = 3;

/// Maximum Providers accepted by a single panel.
pub const MAX_PROVIDER_COUNT: usize = 16;
