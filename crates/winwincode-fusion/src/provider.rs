// SPDX-License-Identifier: Apache-2.0

//! Replaceable Provider boundary and panel routing.
//!
//! The trait shape follows the same portable-future idea as Jev and the
//! Kernel ModelPort, but Fusion owns this interface and does not import those
//! crates. Adapters may wrap local mocks, device runtimes, or remote model
//! APIs without coupling Fusion to any execution kernel.

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use futures::future::BoxFuture;

use crate::FusionProviderAnswer;
use crate::FusionProviderRequest;

/// Provider failure that never carries credentials or raw upstream bodies.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FusionProviderError {
    code: String,
    message: String,
}

impl FusionProviderError {
    #[must_use]
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }

    #[must_use]
    pub fn code(&self) -> &str {
        &self.code
    }

    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for FusionProviderError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for FusionProviderError {}

/// One independent complete Provider adapter.
///
/// Implementations must treat each call as a fresh isolated context: no
/// sibling candidate answers, no shared conversation state across panel
/// candidates.
pub trait FusionProvider: fmt::Debug + Send + Sync {
    fn complete(
        &self,
        request: FusionProviderRequest,
    ) -> BoxFuture<'static, Result<FusionProviderAnswer, FusionProviderError>>;
}

/// Resolves the Provider adapter for one candidate route.
pub trait FusionProviderRouter: fmt::Debug + Send + Sync {
    fn resolve(&self, provider: &str) -> Option<Arc<dyn FusionProvider>>;
}

/// Map-backed router used by hosts and tests.
#[derive(Debug, Default)]
pub struct MapFusionProviderRouter {
    routes: HashMap<String, Arc<dyn FusionProvider>>,
}

impl MapFusionProviderRouter {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn with(mut self, provider: impl Into<String>, adapter: Arc<dyn FusionProvider>) -> Self {
        self.routes.insert(provider.into(), adapter);
        self
    }
}

impl FusionProviderRouter for MapFusionProviderRouter {
    fn resolve(&self, provider: &str) -> Option<Arc<dyn FusionProvider>> {
        self.routes.get(provider).map(Arc::clone)
    }
}
