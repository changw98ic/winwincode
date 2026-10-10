#![allow(clippy::doc_markdown)]

pub mod analysis;
pub mod claims;
pub mod context;
mod contract;
pub mod evidence;
pub mod knowledge;
mod panel;
pub mod planner;
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
pub use panel::validate_panel_result;
pub use provider::FusionProvider;
pub use provider::FusionProviderError;
pub use provider::FusionProviderRouter;
pub use provider::MapFusionProviderRouter;
pub use provider::answer_from_frames;

pub const MIN_PROVIDER_COUNT: usize = 3;

pub const MAX_PROVIDER_COUNT: usize = 16;

pub mod judge;
