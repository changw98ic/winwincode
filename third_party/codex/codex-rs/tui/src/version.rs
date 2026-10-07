/// The current Codex CLI version as embedded at compile time.
pub const CODEX_CLI_VERSION: &str = if cfg!(test) {
    // Keep rendered test fixtures stable across workspace version updates.
    "0.0.0"
} else {
    env!("CARGO_PKG_VERSION")
};
