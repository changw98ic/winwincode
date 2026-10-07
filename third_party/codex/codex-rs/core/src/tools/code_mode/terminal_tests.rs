// SPDX-License-Identifier: Apache-2.0

use super::TerminalHandoffs;
use crate::tools::parallel::ChangeBatchHandoff;
use crate::tools::parallel::ToolContinuation;
use codex_code_mode::CellId;

#[test]
fn a_cancelled_or_closed_cell_cannot_resurrect_a_host_handoff() {
    let handoffs = TerminalHandoffs::default();
    let cell = CellId::new("cell-1".to_string());
    let proposal = ToolContinuation::YieldToHost(ChangeBatchHandoff {
        call_id: "proposal-1".to_string(),
        proposal: "fixture proposal".to_string(),
    });
    assert!(handoffs.publish(cell.clone(), proposal.clone()).is_err());
    handoffs.register(&cell);
    handoffs.publish(cell.clone(), proposal.clone()).unwrap();
    assert!(handoffs.publish(cell.clone(), proposal.clone()).is_err());
    handoffs.close_cell(&cell);
    assert!(
        handoffs.is_pending(&cell),
        "normal close retains an accepted handoff for the outer response"
    );
    handoffs.discard_all();
    assert!(!handoffs.is_pending(&cell));
    assert!(handoffs.publish(cell, proposal.clone()).is_err());

    let next = CellId::new("cell-2".to_string());
    handoffs.register(&next);
    handoffs.close_cell(&next);
    assert!(handoffs.publish(next, proposal).is_err());
}
