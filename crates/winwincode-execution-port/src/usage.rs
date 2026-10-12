// SPDX-License-Identifier: Apache-2.0

use crate::generated::{ExecutionOutcomeUsage, ExecutionOutcomeUsageAccountingStatus};

impl ExecutionOutcomeUsage {
    #[must_use]
    pub const fn unknown(runtime_millis: i64, known_tokens: i64) -> Self {
        Self {
            runtime_millis,
            known_tokens,
            tokens: None,
            cost_microunits: None,
            accounting_status: ExecutionOutcomeUsageAccountingStatus::Unknown,
        }
    }
}

#[must_use]
pub fn valid_usage(usage: &ExecutionOutcomeUsage) -> bool {
    let range = 0..=9_007_199_254_740_991;
    range.contains(&usage.runtime_millis)
        && range.contains(&usage.known_tokens)
        && usage
            .cost_microunits
            .is_none_or(|cost| range.contains(&cost))
        && match usage.accounting_status {
            ExecutionOutcomeUsageAccountingStatus::Known => {
                usage.tokens == Some(usage.known_tokens)
            }
            ExecutionOutcomeUsageAccountingStatus::Unknown => usage.tokens.is_none(),
        }
}
