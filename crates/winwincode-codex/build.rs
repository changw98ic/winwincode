use sha2::{Digest as _, Sha256};

const DEFAULT_RELEASE_PUBLIC_KEY_HEX: &str =
    "197f6b23e16c8532c6abc838facd5ea789be0c76b2920334039bfa8b3d368d61";

fn main() {
    let manifest_dir = std::env::var_os("CARGO_MANIFEST_DIR")
        .expect("Cargo sets CARGO_MANIFEST_DIR while running build scripts");
    let helper_source = std::path::Path::new(&manifest_dir).join("../helper/src/main.rs");
    println!("cargo:rerun-if-changed={}", helper_source.display());
    println!("cargo:rerun-if-env-changed=WINWINCODE_HELPER_RELEASE_PUBLIC_KEY_HEX");
    let source = std::fs::read(&helper_source).expect("helper source is readable");
    println!(
        "cargo:rustc-env=WINWINCODE_HELPER_SOURCE_SHA256=sha256:{:x}",
        Sha256::digest(source)
    );
    let public_key = std::env::var("WINWINCODE_HELPER_RELEASE_PUBLIC_KEY_HEX")
        .unwrap_or_else(|_| DEFAULT_RELEASE_PUBLIC_KEY_HEX.to_owned());
    assert!(
        public_key.len() == 64
            && public_key
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f')),
        "WINWINCODE_HELPER_RELEASE_PUBLIC_KEY_HEX must be 32 lowercase hexadecimal bytes"
    );
    println!("cargo:rustc-env=WINWINCODE_HELPER_RELEASE_PUBLIC_KEY_HEX={public_key}");
}
