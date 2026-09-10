// SPDX-License-Identifier: Apache-2.0
use sha2::{Digest, Sha256};
use std::{fs, process::Command};

fn fixture() -> Vec<u8> {
    include_bytes!("fixtures/delivery-main.json").to_vec()
}
static NEXT_DIR: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
fn temp_dir() -> std::path::PathBuf {
    let p = std::env::temp_dir().join(format!(
        "wwc-cli-{}-{}-{}",
        std::process::id(),
        NEXT_DIR.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&p).unwrap();
    p
}
fn run(bin: &str, dir: &std::path::Path, extra: &[&str]) -> std::process::Output {
    let input = dir.join("input.json");
    let db = dir.join("migration.sqlite");
    let output = dir.join("output.json");
    let backup = dir.join("backup");
    let mut args = vec![
        "--input",
        input.to_str().unwrap(),
        "--db",
        db.to_str().unwrap(),
        "--output",
        output.to_str().unwrap(),
        "--backup-dir",
        backup.to_str().unwrap(),
    ];
    args.extend_from_slice(extra);
    Command::new(bin).args(args).output().unwrap()
}
#[test]
fn cli_backup_receipt_rerun_and_conflict_are_safe() {
    let dir = temp_dir();
    fs::write(dir.join("input.json"), fixture()).unwrap();
    let bin = env!("CARGO_BIN_EXE_migrate_workrun");
    let first = run(bin, &dir, &[]);
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let input = fs::read(dir.join("input.json")).unwrap();
    let backup = dir.join("backup/input.json");
    assert_eq!(fs::read(&backup).unwrap(), input);
    let sidecar =
        String::from_utf8(fs::read(dir.join("backup/input.json.sha256")).unwrap()).unwrap();
    let actual_hash = format!("{:x}", Sha256::digest(&input));
    assert!(sidecar.starts_with(&actual_hash));
    let first_output = fs::read(dir.join("output.json")).unwrap();
    let second = run(bin, &dir, &[]);
    assert!(second.status.success());
    assert!(String::from_utf8_lossy(&second.stdout).contains("already_consumed"));
    assert_eq!(fs::read(dir.join("output.json")).unwrap(), first_output);
    fs::write(dir.join("output.json"), b"conflict").unwrap();
    assert!(!run(bin, &dir, &[]).status.success());
    let restored = dir.join("restored.json");
    fs::copy(&backup, &restored).unwrap();
    assert_eq!(fs::read(restored).unwrap(), input);
    let _ = fs::remove_dir_all(dir);
}
#[test]
fn cli_rejects_missing_or_duplicate_arguments() {
    let dir = temp_dir();
    fs::write(dir.join("input.json"), fixture()).unwrap();
    let bin = env!("CARGO_BIN_EXE_migrate_workrun");
    assert!(
        !Command::new(bin)
            .args(["--input"])
            .output()
            .unwrap()
            .status
            .success()
    );
    assert!(
        !Command::new(bin)
            .args([
                "--input=one",
                "--input=two",
                "--db=x",
                "--output=y",
                "--backup-dir=z"
            ])
            .output()
            .unwrap()
            .status
            .success()
    );
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn cli_rejects_file_aliases_and_preserves_existing_directory_permissions() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let dir = temp_dir();
    let input = dir.join("input.json");
    fs::write(&input, fixture()).unwrap();
    let backup = dir.join("backup");
    fs::create_dir(&backup).unwrap();
    fs::set_permissions(&backup, fs::Permissions::from_mode(0o755)).unwrap();
    let output = dir.join("output.json");
    symlink(&input, &output).unwrap();
    let bin = env!("CARGO_BIN_EXE_migrate_workrun");
    assert!(!run(bin, &dir, &[]).status.success());
    assert_eq!(fs::read(&input).unwrap(), fixture());
    fs::remove_file(&output).unwrap();
    fs::hard_link(&input, &output).unwrap();
    assert!(!run(bin, &dir, &[]).status.success());
    fs::remove_file(&output).unwrap();
    assert!(run(bin, &dir, &[]).status.success());
    assert_eq!(
        fs::metadata(backup).unwrap().permissions().mode() & 0o777,
        0o755
    );
    assert_eq!(
        fs::metadata(output).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(fs::read(input).unwrap(), fixture());
    fs::remove_dir_all(dir).unwrap();
}
