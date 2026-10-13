// SPDX-License-Identifier: Apache-2.0

//! Bounded finite diagnostics for errors erased at the Chat completion boundary.

use std::{io::Write as _, path::Path};

use super::{ProductionCodexError, ProductionCodexErrorKind};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ChatCompletionOperation {
    AcceptedCandidateArtifact,
    RetainCandidateArtifact,
    RetainJobOutcome,
}

impl ChatCompletionOperation {
    const fn name(self) -> &'static str {
        match self {
            Self::AcceptedCandidateArtifact => "accepted_candidate_artifact",
            Self::RetainCandidateArtifact => "retain_candidate_artifact",
            Self::RetainJobOutcome => "retain_job_outcome",
        }
    }
}

#[derive(Default)]
pub(super) struct ChatCompletionDiagnostics {
    observed: Vec<(ChatCompletionOperation, ProductionCodexErrorKind)>,
}

impl ChatCompletionDiagnostics {
    pub(super) fn record<T>(
        &mut self,
        path: &Path,
        is_chat: bool,
        operation: ChatCompletionOperation,
        result: &Result<T, ProductionCodexError>,
    ) {
        let Err(error) = result else { return };
        if !is_chat || self.observed.contains(&(operation, error.kind())) {
            return;
        }
        self.observed.push((operation, error.kind()));
        // Only fixed operations and the typed error kind are persisted. No error
        // text, identities, frames, Provider content or private inputs are read.
        if let Ok(mut log) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            let _ = writeln!(
                log,
                "component=codex stage=chat_completion operation={} kind={:?}",
                operation.name(),
                error.kind(),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ChatCompletionDiagnostics, ChatCompletionOperation, ProductionCodexError,
        ProductionCodexErrorKind,
    };

    fn log_path() -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "wwc-chat-completion-diagnostic-{}.log",
            uuid::Uuid::new_v4()
        ))
    }

    #[test]
    fn chat_errors_record_only_finite_operation_and_kind_once() {
        let path = log_path();
        let mut diagnostics = ChatCompletionDiagnostics::default();
        let result: Result<(), _> = Err(ProductionCodexError::new(
            ProductionCodexErrorKind::ModelBridge,
            "private diagnostic sentinel",
        ));
        for operation in [
            ChatCompletionOperation::AcceptedCandidateArtifact,
            ChatCompletionOperation::RetainCandidateArtifact,
            ChatCompletionOperation::RetainJobOutcome,
        ] {
            diagnostics.record(&path, true, operation, &result);
            diagnostics.record(&path, true, operation, &result);
        }
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            concat!(
                "component=codex stage=chat_completion operation=accepted_candidate_artifact kind=ModelBridge\n",
                "component=codex stage=chat_completion operation=retain_candidate_artifact kind=ModelBridge\n",
                "component=codex stage=chat_completion operation=retain_job_outcome kind=ModelBridge\n",
            )
        );
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn successes_and_other_profiles_do_not_create_diagnostics() {
        let path = log_path();
        let mut diagnostics = ChatCompletionDiagnostics::default();
        let succeeded: Result<(), ProductionCodexError> = Ok(());
        diagnostics.record(
            &path,
            true,
            ChatCompletionOperation::RetainJobOutcome,
            &succeeded,
        );
        let failed: Result<(), _> = Err(ProductionCodexError::new(
            ProductionCodexErrorKind::Conflict,
            "private diagnostic sentinel",
        ));
        diagnostics.record(
            &path,
            false,
            ChatCompletionOperation::RetainJobOutcome,
            &failed,
        );
        assert!(!path.exists());
        assert!(diagnostics.observed.is_empty());
    }

    #[test]
    fn failed_log_write_does_not_change_original_error() {
        let path = log_path().join("missing-parent.log");
        let mut diagnostics = ChatCompletionDiagnostics::default();
        let error = ProductionCodexError::new(
            ProductionCodexErrorKind::Conflict,
            "private diagnostic sentinel",
        );
        let result: Result<(), _> = Err(error.clone());
        diagnostics.record(
            &path,
            true,
            ChatCompletionOperation::RetainJobOutcome,
            &result,
        );
        assert_eq!(result, Err(error));
        assert!(!path.exists());
    }
}
