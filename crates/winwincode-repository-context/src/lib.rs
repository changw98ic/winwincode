//! Baseline-bound repository facts and explicit local code-index capabilities.

mod command_index;
mod git_snapshot;
mod model;
mod scanner;

pub use command_index::CommandLocalCodeIndex;
pub use git_snapshot::GitRepositorySnapshot;
pub use model::{
    CommandPurpose, IndexCapabilities, IndexCapability, LanguageSummary, LocalCodeIndexMode,
    LocalCodeIndexProbe, LocalCodeIndexSnapshot, PackageManager, PackageManagerKind,
    RepositoryCommand, RepositoryContext, RepositoryContextError, RepositoryContextQuery,
    RepositoryFile, RepositoryPaths, TestContext,
};
pub use scanner::{
    FileInventoryLocalCodeIndex, LocalCodeIndexPort, RepositoryContextPort,
    RepositoryContextScanner,
};
