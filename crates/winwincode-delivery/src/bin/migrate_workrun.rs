// SPDX-License-Identifier: Apache-2.0
//! One-shot offline Delivery -> `WorkRun` migration.
//!
//! The input is never modified. A create-new backup and its SHA-256 sidecar are
//! required before opening the receipt database. Re-running the same input
//! returns the durable receipt and safely reuses the output snapshot.
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    env, fs,
    io::Write,
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
};
use winwincode_delivery::{SqliteWorkRunMigration, WorkRunMigrationOutcome};

fn main() {
    if let Err(error) = run() {
        eprintln!("migrate_workrun: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let args = env::args().skip(1).collect::<Vec<_>>();
    validate_args(&args)?;
    let input = required(&args, "--input")?;
    let db = required(&args, "--db")?;
    let output = required(&args, "--output")?;
    let backup_dir = required(&args, "--backup-dir")?;
    let input = PathBuf::from(input);
    let output = PathBuf::from(output);
    let db = PathBuf::from(db);
    let backup_dir = PathBuf::from(backup_dir);
    reject_aliases(&input, &db, &output, &backup_dir)?;
    let bytes = fs::read(&input).map_err(|e| format!("read input: {e}"))?;
    let hash = digest(&bytes);
    let backup = backup_dir.join(input.file_name().ok_or("input has no filename")?);
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&backup_dir)
        .map_err(|e| format!("create backup directory: {e}"))?;
    create_or_verify(&backup, &bytes)?;
    create_or_verify(
        &backup.with_extension("json.sha256"),
        format!("{hash}  {}\n", backup.display()).as_bytes(),
    )?;
    let mut migration = SqliteWorkRunMigration::open(&db).map_err(|e| e.to_string())?;
    let outcome = migration.migrate(&bytes).map_err(|e| e.to_string())?;
    let (status, snapshot) = match outcome {
        WorkRunMigrationOutcome::Applied {
            canonical_snapshot, ..
        } => ("applied", canonical_snapshot),
        WorkRunMigrationOutcome::AlreadyConsumed {
            canonical_snapshot, ..
        } => ("already_consumed", canonical_snapshot),
    };
    create_or_verify(&output, &snapshot)?;
    println!(
        "{}",
        serde_json::to_string(&json!({"status": status, "inputSha256": hash, "output": output}))
            .map_err(|e| e.to_string())?
    );
    Ok(())
}

fn reject_aliases(input: &Path, db: &Path, output: &Path, backup_dir: &Path) -> Result<(), String> {
    let backup = backup_dir.join(input.file_name().ok_or("input has no filename")?);
    let checksum = backup.with_extension("json.sha256");
    let paths = [input, db, output, backup.as_path(), checksum.as_path()];
    for path in paths.iter().copied().chain([backup_dir]) {
        if fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()) {
            return Err(format!("symlink path is not accepted: {}", path.display()));
        }
    }
    let keys = paths
        .iter()
        .map(|p| resolved_path(p))
        .collect::<Result<Vec<_>, _>>()?;
    for (index, left) in paths.iter().enumerate() {
        for (right_index, right) in paths.iter().enumerate().take(index) {
            let same_inode = fs::metadata(left)
                .ok()
                .zip(fs::metadata(right).ok())
                .is_some_and(|(a, b)| a.dev() == b.dev() && a.ino() == b.ino());
            if keys[index] == keys[right_index] || same_inode {
                return Err("input, database, output and backup files must be distinct".into());
            }
        }
    }
    Ok(())
}

fn resolved_path(path: &Path) -> Result<PathBuf, String> {
    if path.exists() {
        return fs::canonicalize(path).map_err(|e| e.to_string());
    }
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let name = path.file_name().ok_or("file path is missing a filename")?;
    Ok(resolved_path(parent)?.join(name))
}

fn validate_args(args: &[String]) -> Result<(), String> {
    let names = ["--input", "--db", "--output", "--backup-dir"];
    let mut seen = std::collections::BTreeSet::new();
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        let name = arg.split('=').next().unwrap_or(arg);
        if !names.contains(&name) || !seen.insert(name) {
            return Err(format!("invalid or duplicate argument {arg}"));
        }
        if arg == name {
            index += 1;
            if index == args.len() || args[index].starts_with("--") {
                return Err(format!("missing value for {name}"));
            }
        }
        index += 1;
    }
    Ok(())
}

fn required(args: &[String], name: &str) -> Result<String, String> {
    for (index, arg) in args.iter().enumerate() {
        if let Some(value) = arg.strip_prefix(&format!("{name}=")) {
            if value.is_empty() {
                return Err(format!("empty value for {name}"));
            }
            return Ok(value.to_owned());
        }
        if arg == name {
            return args
                .get(index + 1)
                .cloned()
                .ok_or_else(|| format!("missing value for {name}"));
        }
    }
    Err(format!("missing {name}=PATH"))
}
fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn create_or_verify(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if let Ok(meta) = fs::symlink_metadata(path) {
        if meta.file_type().is_symlink() {
            return Err(format!("refusing symlink: {}", path.display()));
        }
        let current =
            fs::read(path).map_err(|e| format!("read existing {}: {e}", path.display()))?;
        if current != bytes {
            return Err(format!(
                "existing output/backup differs: {}",
                path.display()
            ));
        }
        return Ok(());
    }
    let parent = path.parent().ok_or("path has no parent")?;
    fs::create_dir_all(parent).map_err(|e| format!("create parent: {e}"))?;
    let temp = parent.join(format!(
        ".{}.{}.part",
        path.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id()
    ));
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true).mode(0o600);
    let mut file = options
        .open(&temp)
        .map_err(|e| format!("create temporary {}: {e}", temp.display()))?;
    if let Err(error) = file.write_all(bytes).and_then(|()| file.sync_all()) {
        let _ = fs::remove_file(&temp);
        return Err(format!("write temporary: {error}"));
    }
    if let Err(error) = fs::hard_link(&temp, path) {
        let _ = fs::remove_file(&temp);
        return Err(format!("publish {}: {error}", path.display()));
    }
    fs::remove_file(&temp).map_err(|e| format!("remove temporary: {e}"))?;
    let parent_file = fs::File::open(parent).map_err(|e| format!("open parent: {e}"))?;
    parent_file
        .sync_all()
        .map_err(|e| format!("sync parent: {e}"))?;
    Ok(())
}
