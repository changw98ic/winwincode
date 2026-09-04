// SPDX-License-Identifier: Apache-2.0

//! Deterministic Worker policy over revision-bound validation diagnostics.
//!
//! Parsing and diagnostic identity live in `winwincode-execution-port`. This
//! module only decides whether the current batch is responsible for a failure;
//! it never invokes a model and never treats a result-only snapshot as proof of
//! a newly introduced diagnostic.

use winwincode_execution_port::{
    diagnostic_parser::dominant_diagnostic_repair_reason,
    generated::{DiagnosticBaselineComparison, ValidationReceiptStatus},
};

/// Stable outcome of comparing one validation result with an exact accepted baseline.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ValidationDiagnosticDisposition {
    /// No current-batch diagnostic requires deterministic repair.
    Pass,
    /// Validation failed but no exact comparable baseline exists.
    BaselineUnavailable,
    /// A deterministic hard rule requires repair without consulting a model.
    RepairRequired { reason_code: &'static str },
}

/// Applies hard diagnostic rules without attributing historical failures when the baseline is
/// absent.
#[must_use]
pub fn decide_validation_diagnostics(
    status: &ValidationReceiptStatus,
    comparison: Option<&DiagnosticBaselineComparison>,
    parser_failed: bool,
) -> ValidationDiagnosticDisposition {
    if matches!(
        status,
        ValidationReceiptStatus::InfrastructureError | ValidationReceiptStatus::Cancelled
    ) {
        return ValidationDiagnosticDisposition::RepairRequired {
            reason_code: "diagnostic.infrastructure",
        };
    }
    if parser_failed {
        return ValidationDiagnosticDisposition::RepairRequired {
            reason_code: "diagnostic.parser_error",
        };
    }
    let Some(comparison) = comparison else {
        return if *status == ValidationReceiptStatus::Passed {
            ValidationDiagnosticDisposition::Pass
        } else {
            ValidationDiagnosticDisposition::BaselineUnavailable
        };
    };
    if comparison.new_count > 0 {
        return ValidationDiagnosticDisposition::RepairRequired {
            reason_code: dominant_diagnostic_repair_reason(comparison),
        };
    }
    ValidationDiagnosticDisposition::Pass
}

#[cfg(test)]
mod tests {
    use winwincode_domain::{Sha256Digest, WorkspaceRevision};
    use winwincode_execution_port::generated::{
        DiagnosticBaselineComparison, DiagnosticCategory, DiagnosticChangeStatus,
        DiagnosticComparisonEntry, DiagnosticParserVersion, DiagnosticSeverity,
        NormalizedDiagnostic, ValidationReceiptStatus,
    };

    use super::{ValidationDiagnosticDisposition, decide_validation_diagnostics};

    #[test]
    fn missing_baseline_never_turns_an_existing_failure_into_batch_blame() {
        assert_eq!(
            decide_validation_diagnostics(&ValidationReceiptStatus::Failed, None, false),
            ValidationDiagnosticDisposition::BaselineUnavailable
        );
        assert_eq!(
            decide_validation_diagnostics(&ValidationReceiptStatus::Passed, None, false),
            ValidationDiagnosticDisposition::Pass
        );
    }

    #[test]
    fn new_hard_diagnostic_and_parser_failure_require_repair_without_a_model() {
        assert_eq!(
            decide_validation_diagnostics(
                &ValidationReceiptStatus::Failed,
                Some(&comparison(DiagnosticChangeStatus::New)),
                false,
            ),
            ValidationDiagnosticDisposition::RepairRequired {
                reason_code: "diagnostic.missing_module"
            }
        );
        assert_eq!(
            decide_validation_diagnostics(&ValidationReceiptStatus::Failed, None, true),
            ValidationDiagnosticDisposition::RepairRequired {
                reason_code: "diagnostic.parser_error"
            }
        );
    }

    #[test]
    fn unchanged_failure_is_not_attributed_but_infrastructure_is_current() {
        assert_eq!(
            decide_validation_diagnostics(
                &ValidationReceiptStatus::Failed,
                Some(&comparison(DiagnosticChangeStatus::Unchanged)),
                false,
            ),
            ValidationDiagnosticDisposition::Pass
        );
        assert_eq!(
            decide_validation_diagnostics(
                &ValidationReceiptStatus::InfrastructureError,
                None,
                true,
            ),
            ValidationDiagnosticDisposition::RepairRequired {
                reason_code: "diagnostic.infrastructure"
            }
        );
    }

    fn comparison(status: DiagnosticChangeStatus) -> DiagnosticBaselineComparison {
        let is_new = status == DiagnosticChangeStatus::New;
        let is_unchanged = status == DiagnosticChangeStatus::Unchanged;
        DiagnosticBaselineComparison {
            baseline_digest: digest('a'),
            base_revision: revision('a'),
            entries: vec![DiagnosticComparisonEntry {
                diagnostic: NormalizedDiagnostic {
                    category: DiagnosticCategory::MissingModule,
                    code: "E0432".to_owned(),
                    column: Some(1),
                    diagnostic_id: digest('c'),
                    display: "module is unavailable".to_owned(),
                    line: Some(1),
                    message_digest: digest('d'),
                    parser_version: DiagnosticParserVersion::CargoJsonV1,
                    path: "src/lib.rs".to_owned(),
                    severity: DiagnosticSeverity::Error,
                },
                status,
            }],
            new_count: i64::from(is_new),
            resolved_count: 0,
            result_digest: digest('b'),
            result_revision: revision('b'),
            unchanged_count: i64::from(is_unchanged),
        }
    }

    fn digest(value: char) -> Sha256Digest {
        Sha256Digest(format!("sha256:{}", value.to_string().repeat(64)))
    }

    fn revision(value: char) -> WorkspaceRevision {
        WorkspaceRevision(format!("git-tree:{}", value.to_string().repeat(40)))
    }
}
