//! Internal child-process entrypoint for embedded Codex helper modes.

fn main() {
    let _path_guard = codex_arg0::arg0_dispatch();
    // The process-owned provider starts its stdio host without arguments.
    if std::env::args_os().nth(1).is_none() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("Code Mode host runtime starts");
        if let Err(error) = runtime.block_on(codex_code_mode_host::run_stdio()) {
            eprintln!("Code Mode host failed: {error}");
            std::process::exit(1);
        }
        return;
    }
    if std::env::args().nth(1).as_deref() == Some("--winwincode-helper-handshake") {
        println!(
            "{{\"protocol\":\"winwincode-kernel-helper\",\"version\":1,\"packageVersion\":\"{}\"}}",
            env!("CARGO_PKG_VERSION")
        );
        return;
    }
    if std::env::args().nth(1).as_deref() == Some("--winwincode-helper-identity") {
        println!(
            "{{\"protocol\":\"winwincode-kernel-helper\",\"version\":1,\"packageVersion\":\"{}\",\"sourceSha256\":\"{}\"}}",
            env!("CARGO_PKG_VERSION"),
            env!("WINWINCODE_HELPER_SOURCE_SHA256")
        );
        return;
    }
    eprintln!("winwincode-kernel-helper is an internal executable");
    std::process::exit(2);
}
