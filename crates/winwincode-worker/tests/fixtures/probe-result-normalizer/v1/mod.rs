// SPDX-License-Identifier: Apache-2.0

//! Runtime-generated test data for the probe result normalizer fixtures.

const DUPLICATE_LOG_OCCURRENCES: usize = 30_000;
const DUPLICATE_LOG_LINE: &[u8] = b"src/probe.ts(12,7): error TS2322: fixture validation failed\n";

/// Generates the large duplicate-log case at test runtime.
pub fn duplicate_log_30k() -> Vec<u8> {
    let mut output = Vec::with_capacity(DUPLICATE_LOG_LINE.len() * DUPLICATE_LOG_OCCURRENCES);
    for _ in 0..DUPLICATE_LOG_OCCURRENCES {
        output.extend_from_slice(DUPLICATE_LOG_LINE);
    }
    output
}
