// SPDX-License-Identifier: Apache-2.0
use std::io::Write as _;
use std::sync::OnceLock;
static DESTINATION: OnceLock<std::path::PathBuf> = OnceLock::new();
pub fn configure(path: std::path::PathBuf) {
    let _ = DESTINATION.set(path);
}
static ORIGIN: OnceLock<std::time::Instant> = OnceLock::new();
pub fn record(stage: &str, details: &serde_json::Value) {
    let path = DESTINATION
        .get()
        .cloned()
        .or_else(|| std::env::var_os("WWC_MECHANISM_TIMING_LOG").map(std::path::PathBuf::from));
    let Some(path) = path else {
        return;
    };
    let origin = ORIGIN.get_or_init(std::time::Instant::now);
    let wall = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_millis());
    let event = serde_json::json!({"instrumentation":"test-support-only","stage":stage,"monotonicMillis":origin.elapsed().as_millis(),"actualWallUnixMillis":wall,"details":details});
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(file, "{event}");
    }
}
