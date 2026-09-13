// SPDX-License-Identifier: Apache-2.0

use std::path::Path;
use std::process::Command;

pub fn candidate_bundle(repository: &Path, base: &str, candidate: &str) -> Vec<u8> {
    let reference = format!("refs/winwincode/candidates/{candidate}");
    let update = Command::new("git")
        .arg("-C")
        .arg(repository)
        .args(["update-ref", &reference, candidate])
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .expect("candidate ref update");
    assert!(
        update.status.success(),
        "candidate ref update: {}",
        String::from_utf8_lossy(&update.stderr)
    );
    let exclude_base = format!("^{base}");
    let bundle = Command::new("git")
        .arg("-C")
        .arg(repository)
        .args(["bundle", "create", "-", &reference, &exclude_base])
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .expect("candidate bundle creation");
    assert!(
        bundle.status.success(),
        "candidate bundle creation: {}",
        String::from_utf8_lossy(&bundle.stderr)
    );
    bundle.stdout
}
