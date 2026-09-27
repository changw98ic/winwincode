// SPDX-License-Identifier: Apache-2.0

use std::process::Command;

fn git(root: &std::path::Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

fn seed_repo(root: &std::path::Path) -> String {
    git(root, &["init", "-q", "-b", "main"]);
    std::fs::write(root.join("frozen.txt"), "v1").expect("write");
    git(root, &["add", "frozen.txt"]);
    git(
        root,
        &[
            "-c",
            "user.email=fixture@example.invalid",
            "-c",
            "user.name=Fixture",
            "commit",
            "-q",
            "-m",
            "base",
        ],
    );
    std::fs::write(root.join("frozen.txt"), "v2").expect("write");
    git(root, &["add", "frozen.txt"]);
    git(
        root,
        &[
            "-c",
            "user.email=fixture@example.invalid",
            "-c",
            "user.name=Fixture",
            "commit",
            "-q",
            "-m",
            "candidate",
        ],
    );
    git(root, &["rev-parse", "HEAD"])
}

#[test]
fn frozen_worktree_is_immune_to_live_workspace_changes() {
    let root = std::env::temp_dir().join(format!("wwc-snap-{}-{}", std::process::id(), 1));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("root");
    let repo = root.join("repo");
    let frozen = root.join("frozen");
    std::fs::create_dir_all(&repo).expect("repo dir");
    let candidate_commit = seed_repo(&repo);

    let snap =
        winwincode_worker::snapshot_worktree::freeze_worktree(&repo, &candidate_commit, &frozen)
            .expect("freeze");
    assert_eq!(snap.commit_id, candidate_commit);

    // Mutate the live workspace after freezing.
    std::fs::write(repo.join("frozen.txt"), "MUTATED").expect("mutate");
    git(&repo, &["add", "frozen.txt"]);
    git(
        &repo,
        &[
            "-c",
            "user.email=fixture@example.invalid",
            "-c",
            "user.name=Fixture",
            "commit",
            "-q",
            "-m",
            "mutate",
        ],
    );

    let frozen_bytes = std::fs::read_to_string(frozen.join("frozen.txt")).expect("read frozen");
    assert_eq!(
        frozen_bytes, "v2",
        "frozen copy must not follow the live workspace"
    );

    winwincode_worker::snapshot_worktree::drop_worktree(&repo, &frozen).expect("drop");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn frozen_worktree_rejects_a_missing_commit() {
    let root = std::env::temp_dir().join(format!("wwc-snap-{}-{}", std::process::id(), 2));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("root");
    let repo = root.join("repo");
    std::fs::create_dir_all(&repo).expect("repo dir");
    seed_repo(&repo);

    let result = winwincode_worker::snapshot_worktree::freeze_worktree(
        &repo,
        "0000000000000000000000000000000000000000",
        &root.join("nope"),
    );
    assert!(result.is_err(), "a foreign commit must be refused");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn frozen_worktree_is_read_only() {
    let root = std::env::temp_dir().join(format!("wwc-snap-{}-{}", std::process::id(), 3));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("root");
    let repo = root.join("repo");
    let frozen = root.join("frozen");
    std::fs::create_dir_all(&repo).expect("repo dir");
    let candidate_commit = seed_repo(&repo);

    let snap =
        winwincode_worker::snapshot_worktree::freeze_worktree(&repo, &candidate_commit, &frozen)
            .expect("freeze");

    // The frozen copy must not report a dirty tree — a verifier cannot write.
    let status = git(
        &repo,
        &["-C", &frozen.to_string_lossy(), "status", "--porcelain"],
    );
    assert_eq!(status, "", "frozen worktree must start clean");

    winwincode_worker::snapshot_worktree::drop_worktree(&repo, &frozen).expect("drop");
    let _ = std::fs::remove_dir_all(&root);
    let _ = snap;
}

#[test]
fn freezing_a_candidate_reports_only_worker_code_identity() {
    let root = std::env::temp_dir().join(format!("wwc-snap-{}-{}", std::process::id(), 4));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("root");
    let repo = root.join("repo");
    let frozen = root.join("frozen");
    std::fs::create_dir_all(&repo).expect("repo dir");
    let candidate_commit = seed_repo(&repo);
    let base_commit = git(&repo, &["rev-parse", &format!("{candidate_commit}^")]);
    let base_tree_id = git(&repo, &["rev-parse", &format!("{base_commit}^{{tree}}")]);

    let worktree =
        winwincode_worker::snapshot_worktree::freeze_worktree(&repo, &candidate_commit, &frozen)
            .expect("freeze");

    assert_eq!(
        git(&repo, &["rev-parse", &format!("{candidate_commit}^")]),
        base_commit
    );
    assert_eq!(
        git(&repo, &["rev-parse", &format!("{base_commit}^{{tree}}")],),
        base_tree_id
    );
    assert_eq!(worktree.commit_id, candidate_commit);
    assert!(!worktree.commit_id.starts_with("snap_"));
    assert!(!worktree.commit_id.starts_with("cnd_"));

    winwincode_worker::snapshot_worktree::drop_worktree(&repo, &frozen).expect("drop");
    let _ = std::fs::remove_dir_all(&root);
}
