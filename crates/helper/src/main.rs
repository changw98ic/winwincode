//! Internal child-process entrypoint for embedded Codex helper modes.

fn main() {
    let _path_guard = codex_arg0::arg0_dispatch();
    eprintln!("winwincode-kernel-helper is an internal executable");
    std::process::exit(2);
}
