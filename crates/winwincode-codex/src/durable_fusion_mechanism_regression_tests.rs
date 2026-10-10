use std::collections::BTreeMap;
use std::process::Command;
use std::sync::Mutex;
use std::time::Instant;

static GIT_OUTPUTS: Mutex<Vec<String>> = Mutex::new(Vec::new());

pub(super) fn record_git_output(verb: &str) {
    GIT_OUTPUTS.lock().unwrap().push(verb.to_string());
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
        // Approximately equal total content bytes; filenames and Git objects remain synthetic.
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
        // Calls the frozen production function. No copied candidate algorithm or mock Git output.
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
