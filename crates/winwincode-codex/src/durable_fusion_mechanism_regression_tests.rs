use std::collections::BTreeMap;
use std::process::Command;
use std::sync::Mutex;
use std::time::Instant;

static GIT_OUTPUTS: Mutex<Vec<String>> = Mutex::new(Vec::new());
thread_local! {
    static THREAD_GIT_OUTPUTS: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
}

pub(super) fn record_git_output(verb: &str) {
    GIT_OUTPUTS.lock().unwrap().push(verb.to_string());
    THREAD_GIT_OUTPUTS.with(|outputs| outputs.borrow_mut().push(verb.to_owned()));
}

fn git(root: &std::path::Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "fixture git failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "mechanism audit: M15 selected Git-process budget"]
async fn mechanism_m15_candidate_context_uses_bounded_git_processes() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let root = std::env::temp_dir().join(format!("wwc-m15-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let mut measured = Vec::new();
    for blobs in [10, 100, 1000] {
        let fixture = root.join(format!("blobs-{blobs}"));
        std::fs::create_dir_all(&fixture).unwrap();
        git(&fixture, &["init", "-q"]);
        let bytes_per_blob = 100_000 / blobs;
        for file in 0..blobs {
            std::fs::write(
                fixture.join(format!("blob-{file:04}")),
                "x".repeat(bytes_per_blob),
            )
            .unwrap();
        }
        git(&fixture, &["add", "."]);
        git(
            &fixture,
            &[
                "-c",
                "user.name=fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit",
                "-qm",
                "synthetic blobs",
            ],
        );
        let commit = git(&fixture, &["rev-parse", "HEAD"]);
        let tree = git(&fixture, &["rev-parse", "HEAD^{tree}"]);
        let heartbeat = Arc::new(AtomicUsize::new(0));
        let heartbeats = Arc::clone(&heartbeat);
        let heartbeat_task = tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                heartbeats.fetch_add(1, Ordering::SeqCst);
            }
        });
        tokio::time::sleep(std::time::Duration::from_millis(3)).await;
        GIT_OUTPUTS.lock().unwrap().clear();
        let before_heartbeat = heartbeat.load(Ordering::SeqCst);
        let start = Instant::now();
        let context = super::candidate_context(
            &fixture,
            &format!("refs/winwincode/candidates/{commit}"),
            &format!("git-tree:{tree}"),
        );
        let elapsed_ms = start.elapsed().as_millis();
        let during_heartbeat = heartbeat.load(Ordering::SeqCst) - before_heartbeat;
        heartbeat_task.abort();
        let _ = heartbeat_task.await;
        let context = context.unwrap();
        assert_eq!(context["files"].as_array().unwrap().len(), blobs);
        let outputs = GIT_OUTPUTS.lock().unwrap().clone();
        let mut verbs = BTreeMap::new();
        for verb in &outputs {
            *verbs.entry(verb.clone()).or_insert(0usize) += 1;
        }
        let row = serde_json::json!({"id":"M15","blobs":blobs,"successful_git_processes":outputs.len(),"verbs":verbs,"elapsed_ms":elapsed_ms,"heartbeat_ticks_during_candidate":during_heartbeat,"source":"actual candidate_context"});
        eprintln!("MECHANISM_RECEIPT {row}");
        measured.push(outputs.len());
    }
    std::fs::remove_dir_all(&root).unwrap();
    assert!(
        measured.iter().all(|count| *count <= 4),
        "candidate read must use bounded Git process count; actual {measured:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn incremental_candidate_context_uses_one_git_blob_batch() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let root = std::env::temp_dir().join(format!("wwc-m15-incremental-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let mut measured = Vec::new();
    for blobs in [10, 100, 1000] {
        let fixture = root.join(format!("blobs-{blobs}"));
        std::fs::create_dir_all(&fixture).unwrap();
        git(&fixture, &["init", "-q"]);
        let bytes_per_blob = 100_000 / blobs;
        for file in 0..blobs {
            std::fs::write(
                fixture.join(format!("blob-{file:04}")),
                "x".repeat(bytes_per_blob),
            )
            .unwrap();
        }
        git(&fixture, &["add", "."]);
        git(
            &fixture,
            &[
                "-c",
                "user.name=fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit",
                "-qm",
                "synthetic blobs",
            ],
        );
        let commit = git(&fixture, &["rev-parse", "HEAD"]);
        let tree = git(&fixture, &["rev-parse", "HEAD^{tree}"]);
        let heartbeat = Arc::new(AtomicUsize::new(0));
        let heartbeats = Arc::clone(&heartbeat);
        let heartbeat_task = tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                heartbeats.fetch_add(1, Ordering::SeqCst);
            }
        });
        tokio::time::sleep(std::time::Duration::from_millis(3)).await;
        THREAD_GIT_OUTPUTS.with(|outputs| outputs.borrow_mut().clear());
        let before_heartbeat = heartbeat.load(Ordering::SeqCst);
        let start = Instant::now();
        let context = super::candidate_context(
            &fixture,
            &format!("refs/winwincode/candidates/{commit}"),
            &format!("git-tree:{tree}"),
        );
        let elapsed_ms = start.elapsed().as_millis();
        let during_heartbeat = heartbeat.load(Ordering::SeqCst) - before_heartbeat;
        heartbeat_task.abort();
        let _ = heartbeat_task.await;
        let context = context.unwrap();
        assert_eq!(context["files"].as_array().unwrap().len(), blobs);
        let outputs = THREAD_GIT_OUTPUTS.with(|outputs| outputs.borrow().clone());
        let mut verbs = BTreeMap::new();
        for verb in &outputs {
            *verbs.entry(verb.clone()).or_insert(0usize) += 1;
        }
        let row = serde_json::json!({"id":"M15","blobs":blobs,"successful_git_processes":outputs.len(),"verbs":verbs,"elapsed_ms":elapsed_ms,"heartbeat_ticks_during_candidate":during_heartbeat,"source":"actual candidate_context"});
        eprintln!("MECHANISM_RECEIPT {row}");
        measured.push(outputs.len());
    }
    std::fs::remove_dir_all(&root).unwrap();
    assert!(
        measured.iter().all(|count| *count == 3),
        "candidate read must use bounded Git process count; actual {measured:?}"
    );
}

#[cfg(unix)]
#[test]
fn incremental_candidate_context_keeps_frozen_binary_symlink_and_gitlink_semantics() {
    use base64::Engine as _;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "wwc-batch-git-contract-{}-{stamp}",
        std::process::id()
    ));
    std::fs::create_dir_all(&root).unwrap();
    git(&root, &["init", "-q"]);
    let binary = vec![0_u8, 255, 254, 13, 10];
    std::fs::write(root.join("note.txt"), "frozen original").unwrap();
    std::fs::write(root.join("binary.bin"), &binary).unwrap();
    std::fs::write(root.join("empty.txt"), []).unwrap();
    std::os::unix::fs::symlink("note.txt", root.join("link")).unwrap();
    git(&root, &["add", "."]);
    git(
        &root,
        &[
            "-c",
            "user.name=fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "-qm",
            "owned frozen files",
        ],
    );
    let first = git(&root, &["rev-parse", "HEAD"]);
    let gitlink = format!("160000,{first},submodule");
    git(&root, &["update-index", "--add", "--cacheinfo", &gitlink]);
    git(
        &root,
        &[
            "-c",
            "user.name=fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "-qm",
            "owned gitlink",
        ],
    );
    let commit = git(&root, &["rev-parse", "HEAD"]);
    let tree = git(&root, &["rev-parse", "HEAD^{tree}"]);
    std::fs::write(root.join("note.txt"), "mutable workspace change").unwrap();
    std::fs::write(root.join("binary.bin"), "changed").unwrap();
    let reference = format!("refs/winwincode/candidates/{commit}");
    let context = super::candidate_context(&root, &reference, &format!("git-tree:{tree}")).unwrap();
    let files = context["files"].as_array().unwrap();
    assert_eq!(files.len(), 5);
    let by_path = |path: &str| files.iter().find(|file| file["path"] == path).unwrap();
    assert_eq!(by_path("note.txt")["content"], "frozen original");
    assert_eq!(by_path("empty.txt")["content"], "");
    assert_eq!(by_path("binary.bin")["encoding"], "base64");
    assert_eq!(
        base64::engine::general_purpose::STANDARD
            .decode(by_path("binary.bin")["content"].as_str().unwrap())
            .unwrap(),
        binary
    );
    assert_eq!(by_path("link")["mode"], "120000");
    assert_eq!(by_path("link")["content"], "note.txt");
    assert_eq!(by_path("submodule")["kind"], "commit");
    assert_eq!(by_path("submodule")["objectId"], first);
    assert!(by_path("submodule").get("content").is_none());
    assert!(super::candidate_context(&root, &reference, "git-tree:foreign").is_err());
    std::fs::remove_dir_all(root).unwrap();
}
