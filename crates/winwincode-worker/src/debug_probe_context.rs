// SPDX-License-Identifier: Apache-2.0

//! Worker assembly for one bounded `DebugProbe` model context.

use winwincode_execution_port::{
    debug_hypothesis_ledger::{
        ValidatedDebugHypothesisLedger, ValidatedDebugHypothesisRoundEvidence,
    },
    debug_probe_delta_context::{
        DebugDeltaContextError, DebugProbeDeltaContextInput, ValidatedDebugProbeDeltaContext,
        prepare_debug_probe_delta_context,
    },
};

use crate::context_safety::WorkerContextSafetyScanner;

/// Trusted values used by the Worker to assemble one model context.
///
/// Source snippets are intentionally absent until the Worker owns an exact
/// current-revision source Artifact reader. D3 raw process output is never an
/// accepted input here.
pub struct WorkerDebugProbeContextInput<'input> {
    /// Latest committed hypothesis Ledger.
    pub current_ledger: &'input ValidatedDebugHypothesisLedger,
    /// Previously prepared context, absent only for the first round.
    pub previous_context: Option<&'input ValidatedDebugProbeDeltaContext>,
    /// Ledger named by `previous_context`, absent only for the first round.
    pub previous_ledger: Option<&'input ValidatedDebugHypothesisLedger>,
    /// Current terminal round receipt and bounded D3 evidence projection.
    pub round_evidence: &'input ValidatedDebugHypothesisRoundEvidence,
}

/// Prepares one Worker-owned context with the fixed safety scanner.
///
/// No scanner, raw text, or source snippet can be supplied by a caller.
///
/// # Errors
///
/// Rejects authority drift, unsafe bounded evidence, invalid cursors, or a
/// context above the fixed serialized/token budget.
pub fn prepare_worker_debug_probe_context(
    input: &WorkerDebugProbeContextInput<'_>,
) -> Result<ValidatedDebugProbeDeltaContext, DebugDeltaContextError> {
    prepare_debug_probe_delta_context(
        &DebugProbeDeltaContextInput {
            current_ledger: input.current_ledger,
            previous_context: input.previous_context,
            previous_ledger: input.previous_ledger,
            round_evidence: input.round_evidence,
            snippets: &[],
        },
        &WorkerContextSafetyScanner,
    )
}
