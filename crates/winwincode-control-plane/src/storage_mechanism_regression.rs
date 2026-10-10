// SPDX-License-Identifier: Apache-2.0

use std::cell::RefCell;

#[derive(Clone, Copy, Debug, Default, serde::Serialize)]
pub(crate) struct Metrics {
    pub ledger_reads: u64,
    pub ledger_read_bytes: u64,
    pub old_event_validations: u64,
    pub ledger_digest_serializations: u64,
    pub ledger_digest_bytes: u64,
    pub ledger_state_serializations: u64,
    pub ledger_state_bytes: u64,
}

thread_local! { static METRICS: RefCell<Option<Metrics>> = const { RefCell::new(None) }; }
pub(crate) fn reset() {
    METRICS.with(|value| *value.borrow_mut() = Some(Metrics::default()));
}
pub(crate) fn finish() -> Metrics {
    METRICS.with(|value| value.borrow_mut().take().unwrap_or_default())
}
fn record(action: impl FnOnce(&mut Metrics)) {
    METRICS.with(|value| {
        if let Some(metrics) = value.borrow_mut().as_mut() {
            action(metrics);
        }
    });
}
pub(crate) fn ledger_read(bytes: usize) {
    record(|metrics| {
        metrics.ledger_reads += 1;
        metrics.ledger_read_bytes += bytes as u64;
    });
}
pub(crate) fn event_validation() {
    record(|metrics| metrics.old_event_validations += 1);
}
pub(crate) fn ledger_digest(bytes: usize) {
    record(|metrics| {
        metrics.ledger_digest_serializations += 1;
        metrics.ledger_digest_bytes += bytes as u64;
    });
}
pub(crate) fn ledger_state(bytes: usize) {
    record(|metrics| {
        metrics.ledger_state_serializations += 1;
        metrics.ledger_state_bytes += bytes as u64;
    });
}

mod fixture {
    // Reuse the canonical local lease/dispatch/WorkRun/SessionBinding fixture.
    include!("../tests/durable_execution_port_ingress.rs");
    include!("storage_runtime_regression.rs");
}
