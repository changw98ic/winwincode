// SPDX-License-Identifier: Apache-2.0

//! Standalone Execution Worker process entrypoint.

fn main() {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("--version") => println!("winwincode-worker {}", env!("CARGO_PKG_VERSION")),
        Some("--check") | None => {
            let identity = winwincode_worker::binary_identity();
            if let Ok(json) = serde_json::to_string(&identity) {
                println!("{json}");
            } else {
                eprintln!("Worker identity serialization failed");
                std::process::exit(1);
            }
        }
        Some(_) => {
            eprintln!("usage: winwincode-worker [--check|--version]");
            std::process::exit(2);
        }
    }
}
