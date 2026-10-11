// SPDX-License-Identifier: Apache-2.0

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

pub struct WorkerDebugProbeContextInput<'input> {
    pub current_ledger: &'input ValidatedDebugHypothesisLedger,
    pub previous_context: Option<&'input ValidatedDebugProbeDeltaContext>,
    pub previous_ledger: Option<&'input ValidatedDebugHypothesisLedger>,
    pub round_evidence: &'input ValidatedDebugHypothesisRoundEvidence,
}

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
