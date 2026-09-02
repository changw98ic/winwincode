use sha2::{Digest as _, Sha256};

fn main() {
    println!("cargo:rerun-if-changed=src/main.rs");
    let source = std::fs::read("src/main.rs").expect("helper source is readable");
    println!(
        "cargo:rustc-env=WINWINCODE_HELPER_SOURCE_SHA256=sha256:{:x}",
        Sha256::digest(source)
    );
}
