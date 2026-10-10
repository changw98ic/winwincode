// SPDX-License-Identifier: Apache-2.0

//! Locates the kernel helper for integration tests.
//!
//! `WWC_TEST_HELPER` wins when set. Otherwise the helper is taken from the
//! same Cargo target/profile directory as the running test executable
//! (`<target>/<profile>/deps/<test>` -> `<target>/<profile>/`), which is where
//! Cargo actually built it. This is correct for an unset, absolute, or
//! relative `CARGO_TARGET_DIR` from any cwd, because it never reinterprets the
//! variable or guesses the caller's working directory.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

const HELPER_NAME: &str = "winwincode-kernel-helper";

pub(crate) fn kernel_helper_path() -> PathBuf {
    resolve_kernel_helper(
        std::env::var_os("WWC_TEST_HELPER"),
        &std::env::current_exe().expect("test executable path"),
    )
}

fn resolve_kernel_helper(explicit: Option<OsString>, test_executable: &Path) -> PathBuf {
    if let Some(explicit) = explicit.filter(|value| !value.is_empty()) {
        return PathBuf::from(explicit);
    }
    profile_directory(test_executable).join(HELPER_NAME)
}

fn profile_directory(test_executable: &Path) -> PathBuf {
    let parent = test_executable
        .parent()
        .expect("test executable has a parent directory");
    if parent.file_name().is_some_and(|name| name == "deps") {
        parent.parent().unwrap_or(parent).to_path_buf()
    } else {
        parent.to_path_buf()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_helper_has_priority() {
        let exe = Path::new("/ws/target/debug/deps/runtime_unified-abc");
        assert_eq!(
            resolve_kernel_helper(Some("/opt/helper".into()), exe),
            PathBuf::from("/opt/helper")
        );
        // An empty override falls back to the build output.
        assert_eq!(
            resolve_kernel_helper(Some(OsString::new()), exe),
            PathBuf::from("/ws/target/debug").join(HELPER_NAME)
        );
    }

    #[test]
    fn helper_follows_the_actual_target_directory() {
        // The test executable path is what Cargo produced for each setting:
        // CARGO_TARGET_DIR unset, absolute, `target` from the workspace cwd,
        // and `target` from the crate cwd.
        for (exe, profile) in [
            ("/ws/target/debug/deps/t-1", "/ws/target/debug"),
            ("/abs/out/debug/deps/t-1", "/abs/out/debug"),
            ("/ws/target/debug/deps/t-1", "/ws/target/debug"),
            (
                "/ws/crates/winwincode-worker/target/debug/deps/t-1",
                "/ws/crates/winwincode-worker/target/debug",
            ),
            ("/ws/target/release/deps/t-1", "/ws/target/release"),
        ] {
            assert_eq!(
                resolve_kernel_helper(None, Path::new(exe)),
                Path::new(profile).join(HELPER_NAME),
                "{exe}"
            );
        }
    }

    #[test]
    fn live_resolution_matches_this_test_binary_build_output() {
        let exe = std::env::current_exe().unwrap();
        let resolved = resolve_kernel_helper(None, &exe);
        // Same profile directory that holds this test binary's deps/.
        assert_eq!(
            resolved.parent().unwrap(),
            exe.parent().unwrap().parent().unwrap()
        );
        assert!(resolved.ends_with(HELPER_NAME));
    }
}
