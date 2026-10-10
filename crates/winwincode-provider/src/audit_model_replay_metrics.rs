// SPDX-License-Identifier: Apache-2.0
//! Feature-gated offline audit counters for actual Device replay JSON reads.
use serde::Serialize;
use std::cell::RefCell;
#[derive(Clone, Debug, Default, Serialize)]
pub struct ReplayMetrics {
    pub select_chunks_rows: u64,
    pub chunks_json_bytes_deserialized: u64,
}
thread_local! { static METRICS: RefCell<ReplayMetrics> = RefCell::new(ReplayMetrics::default()); }
pub fn reset() {
    METRICS.with(|x| *x.borrow_mut() = ReplayMetrics::default());
}
pub fn take() -> ReplayMetrics {
    METRICS.with(|x| std::mem::take(&mut *x.borrow_mut()))
}
pub(crate) fn record(bytes: usize) {
    METRICS.with(|x| {
        let mut x = x.borrow_mut();
        x.select_chunks_rows += 1;
        x.chunks_json_bytes_deserialized += bytes as u64;
    });
}
