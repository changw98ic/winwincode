// SPDX-License-Identifier: Apache-2.0
//! Offline audit counters. Counts actual reads, writes and checked frames, not SQL estimates.
use serde::Serialize;
use std::cell::RefCell;

#[derive(Clone, Debug, Default, Serialize)]
pub(crate) struct Metrics {
    pub(crate) full_frame_rows_read: u64,
    pub(crate) full_frame_json_bytes_read: u64,
    pub(crate) cursor_reads: u64,
    pub(crate) cursor_json_bytes_read: u64,
    pub(crate) cursor_writes: u64,
    pub(crate) cursor_json_bytes_written: u64,
    pub(crate) fingerprint_visits: u64,
}
thread_local! { static METRICS: RefCell<Metrics> = RefCell::new(Metrics::default()); }
pub(crate) fn reset() {
    METRICS.with(|x| *x.borrow_mut() = Metrics::default());
}
pub(crate) fn take() -> Metrics {
    METRICS.with(|x| std::mem::take(&mut *x.borrow_mut()))
}
pub(crate) fn record_frame_read(bytes: usize) {
    METRICS.with(|x| {
        let mut x = x.borrow_mut();
        x.full_frame_rows_read += 1;
        x.full_frame_json_bytes_read += bytes as u64;
    });
}
pub(crate) fn record_cursor_read(bytes: usize) {
    METRICS.with(|x| {
        let mut x = x.borrow_mut();
        x.cursor_reads += 1;
        x.cursor_json_bytes_read += bytes as u64;
    });
}
pub(crate) fn record_cursor_write(bytes: usize) {
    METRICS.with(|x| {
        let mut x = x.borrow_mut();
        x.cursor_writes += 1;
        x.cursor_json_bytes_written += bytes as u64;
    });
}
pub(crate) fn record_fingerprint_visit() {
    METRICS.with(|x| x.borrow_mut().fingerprint_visits += 1);
}

pub(crate) fn report(name: &str, value: &serde_json::Value) {
    println!("MODEL_MECHANISM_AUDIT {value}");
    if let Some(root) = std::env::var_os("WWC_MECHANISM_AUDIT_OUTPUT") {
        let root = std::path::Path::new(&root);
        std::fs::create_dir_all(root).unwrap();
        std::fs::write(
            root.join(format!("{name}.json")),
            serde_json::to_vec_pretty(value).unwrap(),
        )
        .unwrap();
    }
}
