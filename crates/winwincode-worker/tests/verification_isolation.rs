// SPDX-License-Identifier: Apache-2.0

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use tempfile::TempDir;
use winwincode_worker::verification_isolation::{
    PROTECTED_INPUT_SCOPE_VERSION, ProtectedInputScope, ProtectionCapability,
    VerificationIsolation, VerificationScratch,
};

fn shell_script(protected: &str) -> String {
    format!(
        r#"
set -eu
blocked() {{ ! "$@"; }}
blocked /bin/sh -c 'printf created > "$WWC_PROBE_CREATED"'
blocked /bin/sh -c 'printf modified > "$WWC_PROBE_SOURCE"'
blocked /bin/sh -c 'rm -f "$WWC_PROBE_SOURCE"'
blocked /bin/sh -c 'mv "$WWC_PROBE_SOURCE" "$WWC_PROBE_SOURCE.renamed"'
blocked /bin/sh -c 'printf replacement > "$WWC_SCRATCH/replacement" && mv "$WWC_SCRATCH/replacement" "$WWC_PROBE_SOURCE"'
blocked /bin/sh -c 'cp "$WWC_PROBE_SOURCE" "$WWC_SCRATCH/original" && printf changed > "$WWC_PROBE_SOURCE" && cp "$WWC_SCRATCH/original" "$WWC_PROBE_SOURCE"'
blocked /bin/sh -c 'ln -s "$WWC_PROBE_SOURCE" "$WWC_SCRATCH/source-link" && printf changed > "$WWC_SCRATCH/source-link"'
blocked /bin/sh -c 'ln "$WWC_PROBE_SOURCE" "$WWC_SCRATCH/source-hardlink" && printf changed > "$WWC_SCRATCH/source-hardlink"'
blocked /bin/sh -c '/bin/sh -c '\''printf child > "$WWC_PROBE_CREATED_BY_CHILD"'\'''
printf ok > "$WWC_SCRATCH/direct-ok"
test "{protected}" = "$(cat "$WWC_PROBE_SOURCE")"
test ! -e "$WWC_PROBE_CREATED"
test ! -e "$WWC_PROBE_CREATED_BY_CHILD"
test ! -e "$WWC_PROBE_SOURCE.renamed"
test -e "$WWC_SCRATCH/direct-ok"
"#
    )
}

fn isolation_fixture(name: &str) -> (TempDir, ProtectedInputScope, VerificationScratch, PathBuf) {
    let root = tempfile::tempdir().unwrap_or_else(|_| panic!("create {name} root"));
    let checkout = root.path().join("checkout");
    let workspace = root.path().join("workspace");
    fs::create_dir_all(checkout.join("src")).expect("create source");
    fs::create_dir_all(checkout.join("tests")).expect("create tests");
    fs::create_dir_all(checkout.join(".winwincode")).expect("create controlled config");
    fs::create_dir_all(&workspace).expect("create workspace");
    fs::write(checkout.join("src/source.txt"), b"sealed\n").expect("write source");
    fs::write(checkout.join("tests/test.txt"), b"test\n").expect("write test");
    fs::write(checkout.join("build.sh"), b"#!/bin/sh\n").expect("write build script");
    fs::write(checkout.join("package-lock.json"), b"{}\n").expect("write lockfile");
    fs::write(
        checkout.join(".winwincode/validation.toml"),
        b"schemaVersion = 1\n",
    )
    .expect("write controlled config");
    let scope = ProtectedInputScope::new(&checkout, &["src".to_owned(), "tests".to_owned()])
        .expect("create protected scope");
    let scratch = VerificationScratch::create(&workspace, name, 7).expect("create scratch");
    (root, scope, scratch, checkout)
}

fn helper_executable() -> Option<PathBuf> {
    std::env::var_os("WWC_WORKER_HELPER_EXECUTABLE").map(PathBuf::from)
}

#[test]
fn protected_input_scope_is_versioned_and_covers_required_categories() {
    let (_root, scope, _scratch, checkout) = isolation_fixture("scope");

    assert_eq!(scope.version(), PROTECTED_INPUT_SCOPE_VERSION);
    assert_eq!(scope.root(), fs::canonicalize(checkout).unwrap());
    assert_eq!(
        scope.categories(),
        [
            "source",
            "tests",
            "build-scripts",
            "lockfiles",
            "controlled-config"
        ]
    );
    assert!(!scope.digest().is_empty());
}

#[tokio::test]
async fn verification_runtime_blocks_all_mutations_during_execution() {
    let (_root, scope, scratch, checkout) = isolation_fixture("mutations");
    let isolation = VerificationIsolation::open(scope, scratch, helper_executable())
        .await
        .expect("open enforced verification isolation");
    assert!(
        isolation.capability().allows_strong_pass(),
        "{:?}",
        isolation.capability()
    );

    let mut environment = HashMap::new();
    environment.insert(
        "WWC_PROBE_SOURCE".to_owned(),
        checkout
            .join("src/source.txt")
            .to_string_lossy()
            .into_owned(),
    );
    environment.insert(
        "WWC_PROBE_CREATED".to_owned(),
        checkout
            .join("src/created.txt")
            .to_string_lossy()
            .into_owned(),
    );
    environment.insert(
        "WWC_PROBE_CREATED_BY_CHILD".to_owned(),
        checkout
            .join("tests/created-by-child.txt")
            .to_string_lossy()
            .into_owned(),
    );
    let output = isolation
        .run(
            &[
                "/bin/sh".to_owned(),
                "-c".to_owned(),
                shell_script("sealed"),
            ],
            &checkout,
            environment,
            Duration::from_secs(10),
            1_048_576,
        )
        .await
        .expect("run mutation attacks");

    assert!(output.status.success(), "{}", output.stderr_lossy());
    assert_eq!(
        fs::read_to_string(checkout.join("src/source.txt")).unwrap(),
        "sealed\n"
    );
    assert!(!checkout.join("src/created.txt").exists());
    assert!(!checkout.join("tests/created-by-child.txt").exists());
}

#[tokio::test]
async fn build_cache_temp_and_output_writes_are_confined_to_scratch() {
    let (_root, scope, scratch, checkout) = isolation_fixture("scratch-writes");
    let isolation = VerificationIsolation::open(scope, scratch.clone(), helper_executable())
        .await
        .expect("open enforced verification isolation");
    let script = r#"set -eu
printf home > "$HOME/home.out"
printf temp > "$TMPDIR/temp.out"
printf build > "$CARGO_TARGET_DIR/build.out"
printf cache > "$WWC_CACHE_DIR/cache.out"
printf output > "$WWC_OUTPUT_DIR/output.out"
test "$HOME" = "$WWC_SCRATCH/home"
test "$TMPDIR" = "$WWC_SCRATCH/tmp"
"#;
    let output = isolation
        .run(
            &["/bin/sh".to_owned(), "-c".to_owned(), script.to_owned()],
            &checkout,
            HashMap::new(),
            Duration::from_secs(10),
            1_048_576,
        )
        .await
        .expect("write scratch outputs");

    assert!(output.status.success(), "{}", output.stderr_lossy());
    assert_eq!(fs::read(scratch.home().join("home.out")).unwrap(), b"home");
    assert_eq!(
        fs::read(scratch.temporary().join("temp.out")).unwrap(),
        b"temp"
    );
    assert_eq!(
        fs::read(scratch.build().join("build.out")).unwrap(),
        b"build"
    );
    assert_eq!(
        fs::read(scratch.cache().join("cache.out")).unwrap(),
        b"cache"
    );
    assert_eq!(
        fs::read(scratch.output().join("output.out")).unwrap(),
        b"output"
    );
}

#[tokio::test]
async fn concurrent_verification_runs_never_share_scratch() {
    let (_root, scope, first, checkout) = isolation_fixture("scratch-first");
    let second = first.sibling("scratch-first", 7).expect("second scratch");
    let sentinels = [
        ".winwincode-isolation-probe",
        ".winwincode-isolation-created",
        ".winwincode-isolation-child",
        ".winwincode-isolation-grandchild",
    ];
    for name in sentinels {
        fs::write(checkout.join(name), b"original snapshot content").unwrap();
    }
    let (first_isolation, second_isolation) = tokio::join!(
        VerificationIsolation::open(scope.clone(), first.clone(), helper_executable()),
        VerificationIsolation::open(scope, second.clone(), helper_executable()),
    );
    let first_isolation = first_isolation.expect("open first verification isolation");
    let second_isolation = second_isolation.expect("open second verification isolation");
    for name in sentinels {
        assert_eq!(
            fs::read(checkout.join(name)).unwrap(),
            b"original snapshot content"
        );
    }
    let first_script = r#"set -eu
printf first > "$WWC_SCRATCH/private"
test ! -e "$WWC_SCRATCH/other"
"#;
    let second_script = r#"set -eu
printf second > "$WWC_SCRATCH/other"
test ! -e "$WWC_SCRATCH/private"
"#;
    let first_argv = [
        "/bin/sh".to_owned(),
        "-c".to_owned(),
        first_script.to_owned(),
    ];
    let second_argv = [
        "/bin/sh".to_owned(),
        "-c".to_owned(),
        second_script.to_owned(),
    ];
    let (first_output, second_output) = tokio::join!(
        first_isolation.run(
            &first_argv,
            &checkout,
            HashMap::new(),
            Duration::from_secs(10),
            1_048_576,
        ),
        second_isolation.run(
            &second_argv,
            &checkout,
            HashMap::new(),
            Duration::from_secs(10),
            1_048_576,
        ),
    );

    let first_output = first_output.expect("run first scratch");
    let second_output = second_output.expect("run second scratch");
    assert!(
        first_output.status.success(),
        "{}",
        first_output.stderr_lossy()
    );
    assert!(
        second_output.status.success(),
        "{}",
        second_output.stderr_lossy()
    );
    assert_eq!(fs::read(first.root().join("private")).unwrap(), b"first");
    assert_eq!(fs::read(second.root().join("other")).unwrap(), b"second");
    assert!(!first.root().join("other").exists());
    assert!(!second.root().join("private").exists());
}

#[tokio::test]
async fn process_group_cleanup_kills_background_descendants() {
    let (_root, scope, scratch, checkout) = isolation_fixture("process-group");
    let isolation = VerificationIsolation::open(scope, scratch.clone(), helper_executable())
        .await
        .expect("open process-group isolation");
    let script = r#"set -eu
/bin/sh -c 'sleep 1; printf leaked > "$WWC_SCRATCH/leaked"' >/dev/null 2>&1 &
exit 0
"#;
    let output = isolation
        .run(
            &["/bin/sh".to_owned(), "-c".to_owned(), script.to_owned()],
            &checkout,
            HashMap::new(),
            Duration::from_secs(10),
            1_048_576,
        )
        .await
        .expect("run background descendant");
    assert!(output.status.success(), "{}", output.stderr_lossy());
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!scratch.root().join("leaked").exists());
}

#[tokio::test]
async fn timeout_terminates_the_entire_process_group() {
    let (_root, scope, scratch, checkout) = isolation_fixture("timeout-process-group");
    let isolation = VerificationIsolation::open(scope, scratch.clone(), helper_executable())
        .await
        .expect("open timeout process-group isolation");
    let script = r#"set -eu
/bin/sh -c 'sleep 1; printf leaked > "$WWC_SCRATCH/timeout-leaked"' >/dev/null 2>&1 &
sleep 2
"#;
    let output = isolation
        .run(
            &["/bin/sh".to_owned(), "-c".to_owned(), script.to_owned()],
            &checkout,
            HashMap::new(),
            Duration::from_millis(100),
            1_048_576,
        )
        .await
        .expect("run command past its deadline");

    assert!(output.timed_out);
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!scratch.root().join("timeout-leaked").exists());
}

#[tokio::test]
async fn output_overflow_terminates_the_entire_process_group() {
    let (_root, scope, scratch, checkout) = isolation_fixture("overflow-process-group");
    let isolation = VerificationIsolation::open(scope, scratch.clone(), helper_executable())
        .await
        .expect("open overflow process-group isolation");
    let script = r#"set -eu
/bin/sh -c 'sleep 1; printf leaked > "$WWC_SCRATCH/overflow-leaked"' >/dev/null 2>&1 &
printf '%s' '0123456789012345678901234567890123456789'
sleep 2
"#;
    let output = isolation
        .run(
            &["/bin/sh".to_owned(), "-c".to_owned(), script.to_owned()],
            &checkout,
            HashMap::new(),
            Duration::from_secs(2),
            16,
        )
        .await
        .expect("run command past its output limit");

    assert!(output.overflowed);
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!scratch.root().join("overflow-leaked").exists());
}

#[tokio::test]
async fn protected_scope_rejects_symlink_escape_and_overlapping_scratch() {
    let root = tempfile::tempdir().unwrap_or_else(|_| panic!("create path-boundary root"));
    let checkout = root.path().join("checkout");
    let outside = root.path().join("outside");
    fs::create_dir_all(&checkout).unwrap();
    fs::create_dir_all(&outside).unwrap();
    std::os::unix::fs::symlink(&outside, checkout.join("escape")).expect("create escape symlink");
    assert!(ProtectedInputScope::new(&checkout, &["escape".to_owned()]).is_err());

    let scope = ProtectedInputScope::new(&checkout, &[]).expect("create scope");
    let scratch =
        VerificationScratch::create(&checkout, "overlap", 0).expect("create overlapping scratch");
    let overlap = VerificationIsolation::open(scope, scratch, helper_executable()).await;
    assert!(overlap.is_err());
}

#[tokio::test]
async fn unavailable_protection_never_allows_a_strong_pass_for_supported_backends() {
    let (_root, scope, scratch, checkout) = isolation_fixture("capability");

    for backend in ["macos-seatbelt-v1", "linux-codex-sandbox-v1"] {
        let capability = ProtectionCapability::Unavailable {
            backend: backend.to_owned(),
            reason: "forced capability failure".to_owned(),
        };

        assert!(!capability.allows_strong_pass());
        assert!(!VerificationIsolation::allows_strong_pass_with(&capability));
    }
    fs::remove_dir_all(&checkout).expect("remove input before the capability probe");
    let isolation = VerificationIsolation::open(scope, scratch.clone(), helper_executable())
        .await
        .expect("retain unavailable capability");
    assert!(!isolation.capability().allows_strong_pass());
    let command = isolation.command(
        &[
            "/bin/sh".into(),
            "-c".into(),
            "printf pass > \"$WWC_SCRATCH/false-pass\"".into(),
        ],
        &checkout,
        HashMap::new(),
    );
    assert!(command.is_err());
    assert!(!scratch.root().join("false-pass").exists());
}
