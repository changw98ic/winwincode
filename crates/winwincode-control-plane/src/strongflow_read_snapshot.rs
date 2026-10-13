// SPDX-License-Identifier: Apache-2.0

use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use winwincode_storage::{
    ArtifactError, ArtifactErrorKind, ArtifactObject, GitSourceResolver, SqliteStorage,
    ValidatedGitCandidateDiff, ValidatedGitCandidateFileContent, ValidatedGitCandidateReview,
    ValidatedGitSourceArtifact,
};

use crate::ControlPlane;
use crate::strongflow_projection::StrongFlowProjectionError;

pub struct StrongFlowReadSnapshot {
    context: ControlPlane,
}

impl Deref for StrongFlowReadSnapshot {
    type Target = ControlPlane;

    fn deref(&self) -> &Self::Target {
        &self.context
    }
}

impl Drop for StrongFlowReadSnapshot {
    fn drop(&mut self) {
        if let Some(artifacts) = self.context.artifact_store.take() {
            let _ = artifacts.close();
        }
        if let Some(storage) = self.context.storage.take() {
            let _ = storage.close();
        }
    }
}

impl ControlPlane {
    pub fn strongflow_read_snapshot(
        &self,
    ) -> Result<StrongFlowReadSnapshot, StrongFlowProjectionError> {
        let unavailable = || {
            StrongFlowProjectionError::ServiceUnavailable(
                "StrongFlow read resources are unavailable".to_owned(),
            )
        };
        self.storage_ref().map_err(|_| unavailable())?;
        let path = self.local_database_path.as_ref().ok_or_else(unavailable)?;
        let storage = SqliteStorage::open_read_snapshot(path).map_err(|_| unavailable())?;
        let artifacts = self
            .artifact_store
            .as_ref()
            .map(winwincode_storage::ArtifactStore::read_snapshot)
            .transpose()
            .map_err(|_| unavailable())?;
        let resolver = self.git_source_read_handle.clone();
        if self.git_source_resolver.is_some() && resolver.is_none() {
            return Err(unavailable());
        }
        Ok(StrongFlowReadSnapshot {
            context: ControlPlane {
                outcome_recovery_cursor: String::new(),
                storage: Some(Box::new(storage)),
                local_database_path: Some(path.clone()),
                audit_store: None,
                artifact_store: artifacts,
                git_source_resolver: resolver
                    .clone()
                    .map(|resolver| Box::new(resolver) as Box<dyn GitSourceResolver>),
                git_source_read_handle: resolver,
                git_repository_root: self.git_repository_root.clone(),
                publisher: None,
                temporary_root: None,
                strongflow_sources: self.strongflow_sources.clone(),
                delivery_authority: None,
                delivery_dispatcher: None,
                publication_authority: None,
                publication_providers: None,
            },
        })
    }
}

#[derive(Clone)]
pub(crate) struct SharedGitSourceResolver {
    resolver: Arc<Mutex<Box<dyn GitSourceResolver>>>,
    root: Option<PathBuf>,
}

impl SharedGitSourceResolver {
    pub(crate) fn new(resolver: Box<dyn GitSourceResolver>) -> Self {
        Self {
            root: resolver.controlled_repository_root().map(Path::to_path_buf),
            resolver: Arc::new(Mutex::new(resolver)),
        }
    }

    fn with_resolver<T>(
        &self,
        operation: impl FnOnce(&dyn GitSourceResolver) -> Result<T, ArtifactError>,
    ) -> Result<T, ArtifactError> {
        let guard = self
            .resolver
            .lock()
            .map_err(|_| ArtifactError::object_adapter(ArtifactErrorKind::Adapter))?;
        operation(guard.as_ref())
    }
}

impl GitSourceResolver for SharedGitSourceResolver {
    fn resolve_candidate(
        &self,
        artifact: &ArtifactObject,
        repository_locator: &str,
        base_revision: &str,
    ) -> Result<ValidatedGitSourceArtifact, ArtifactError> {
        self.with_resolver(|resolver| {
            resolver.resolve_candidate(artifact, repository_locator, base_revision)
        })
    }

    fn controlled_repository_root(&self) -> Option<&Path> {
        self.root.as_deref()
    }

    fn candidate_review(
        &self,
        source: &ValidatedGitSourceArtifact,
    ) -> Result<ValidatedGitCandidateReview, ArtifactError> {
        self.with_resolver(|resolver| resolver.candidate_review(source))
    }

    fn candidate_diff(
        &self,
        source: &ValidatedGitSourceArtifact,
        path: &str,
    ) -> Result<ValidatedGitCandidateDiff, ArtifactError> {
        self.with_resolver(|resolver| resolver.candidate_diff(source, path))
    }

    fn candidate_file_content(
        &self,
        source: &ValidatedGitSourceArtifact,
        path: &str,
    ) -> Result<ValidatedGitCandidateFileContent, ArtifactError> {
        self.with_resolver(|resolver| resolver.candidate_file_content(source, path))
    }
}
