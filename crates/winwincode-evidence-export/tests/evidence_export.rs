// SPDX-License-Identifier: Apache-2.0

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use sha2::{Digest, Sha256};
use winwincode_evidence_export::{
    ArtifactSource, DocumentKind, EvidenceDocument, EvidenceErrorKind, ExportCapacity,
    ExportClassification, ExportRequest, TraceRecord, TraceSource, export_evidence,
    verify_evidence_archive, verify_evidence_package,
};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(1);

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new(label: &str) -> Self {
        let id = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "winwincode-evidence-{label}-{}-{id}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("create test directory");
        Self(path)
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .fold(String::with_capacity(64), |mut output, byte| {
            write!(output, "{byte:02x}").expect("write digest");
            output
        })
}

fn document(kind: DocumentKind, bytes: &[u8]) -> EvidenceDocument {
    EvidenceDocument {
        kind,
        bytes: bytes.to_vec(),
        expected_sha256: digest(bytes),
    }
}

fn request(artifact_path: &Path, package_id: &str) -> ExportRequest {
    let artifact_bytes = fs::read(artifact_path).expect("read Artifact fixture");
    ExportRequest {
        package_id: package_id.to_owned(),
        source_commit: "a".repeat(40),
        trace_records: vec![
            TraceRecord {
                source: TraceSource::Audit,
                occurred_at_millis: 20,
                sequence: 2,
                record_id: "audit-2".into(),
                scope_id: "workspace-1".into(),
                kind: "delivery.verified".into(),
                content_digest: "c".repeat(64),
            },
            TraceRecord {
                source: TraceSource::Delivery,
                occurred_at_millis: 10,
                sequence: 1,
                record_id: "delivery-1".into(),
                scope_id: "workspace-1".into(),
                kind: "candidate.created".into(),
                content_digest: "b".repeat(64),
            },
            TraceRecord {
                source: TraceSource::WorkerRuntime,
                occurred_at_millis: 12,
                sequence: 3,
                record_id: "runtime-3".into(),
                scope_id: "workspace-1".into(),
                kind: "execution.completed".into(),
                content_digest: "d".repeat(64),
            },
            TraceRecord {
                source: TraceSource::Artifact,
                occurred_at_millis: 15,
                sequence: 4,
                record_id: "artifact-4".into(),
                scope_id: "workspace-1".into(),
                kind: "artifact.finalized".into(),
                content_digest: "e".repeat(64),
            },
        ],
        documents: vec![
            document(DocumentKind::Verdict, br#"{"status":"pass"}\n"#),
            document(
                DocumentKind::MergeGuide,
                b"Apply patch.diff, then verify.\n",
            ),
            document(DocumentKind::PatchDiff, b"diff --git a/a b/a\n+result\n"),
        ],
        artifacts: vec![ArtifactSource {
            artifact_id: "art_001".into(),
            logical_name: "test.log".into(),
            source_path: artifact_path.to_path_buf(),
            expected_sha256: digest(&artifact_bytes),
            expected_bytes: artifact_bytes.len() as u64,
            classification: ExportClassification::Confidential,
        }],
        capacity: ExportCapacity {
            available_bytes: 10_000_000,
            reserve_bytes: 1_000,
            warning_below_bytes: 10_000_000,
        },
        create_archive: true,
    }
}

#[test]
fn exports_stable_offline_verifiable_package_and_archive() {
    let fixture = TestDirectory::new("stable");
    let artifact = fixture.0.join("runtime.log");
    fs::write(&artifact, b"worker completed with status pass\n").expect("write Artifact fixture");
    let first_root = fixture.0.join("first");
    let second_root = fixture.0.join("second");

    let first =
        export_evidence(&first_root, &request(&artifact, "evidence-001")).expect("first export");
    let second =
        export_evidence(&second_root, &request(&artifact, "evidence-001")).expect("second export");

    let first_manifest =
        verify_evidence_package(&first.package_path).expect("verify first package");
    let second_manifest =
        verify_evidence_package(&second.package_path).expect("verify second package");
    assert_eq!(first_manifest, second_manifest);
    assert_eq!(first_manifest.trace_record_count, 4);
    assert_eq!(first.manifest_sha256, second.manifest_sha256);
    assert!(first.disk_warning);
    assert_eq!(
        fs::read(first.archive_path.as_ref().expect("first archive")).expect("read first archive"),
        fs::read(second.archive_path.as_ref().expect("second archive"))
            .expect("read second archive")
    );
    assert_eq!(
        verify_evidence_archive(first.archive_path.as_ref().expect("archive"))
            .expect("verify archive"),
        first_manifest
    );
}

#[test]
fn insufficient_disk_fails_before_any_package_is_created() {
    let fixture = TestDirectory::new("capacity");
    let artifact = fixture.0.join("runtime.log");
    fs::write(&artifact, b"large enough\n").expect("write Artifact fixture");
    let output = fixture.0.join("output");
    let mut request = request(&artifact, "evidence-capacity");
    request.capacity.available_bytes = 1;

    let error = export_evidence(&output, &request).expect_err("disk budget must fail");
    assert_eq!(error.kind(), EvidenceErrorKind::InsufficientDisk);
    assert!(!output.exists());
}

#[test]
fn secrets_and_changed_artifacts_are_rejected_before_publication() {
    let fixture = TestDirectory::new("rejection");
    let artifact = fixture.0.join("runtime.log");
    fs::write(&artifact, b"worker completed\n").expect("write Artifact fixture");

    let mut secret = request(&artifact, "evidence-secret");
    secret.documents = vec![
        document(DocumentKind::PatchDiff, b"password=do-not-export\n"),
        document(DocumentKind::Verdict, b"{}\n"),
        document(DocumentKind::MergeGuide, b"merge\n"),
    ];
    let error =
        export_evidence(&fixture.0.join("secret"), &secret).expect_err("secret document must fail");
    assert_eq!(error.kind(), EvidenceErrorKind::SecretDetected);

    let mut changed = request(&artifact, "evidence-changed");
    changed.artifacts[0].expected_sha256 = "f".repeat(64);
    let error = export_evidence(&fixture.0.join("changed"), &changed)
        .expect_err("Artifact digest must fail");
    assert_eq!(error.kind(), EvidenceErrorKind::DigestMismatch);
    assert!(!fixture.0.join("secret").exists());
    assert!(!fixture.0.join("changed").exists());
}

#[test]
fn streaming_secret_scan_detects_a_marker_across_copy_chunks() {
    let fixture = TestDirectory::new("stream-secret");
    let artifact = fixture.0.join("runtime.log");
    let mut bytes = vec![b'x'; (64 * 1024) - 4];
    bytes.extend_from_slice(b"password=hidden\n");
    fs::write(&artifact, &bytes).expect("write Artifact fixture");

    let error = export_evidence(
        &fixture.0.join("output"),
        &request(&artifact, "evidence-stream-secret"),
    )
    .expect_err("secret split across chunks must fail");
    assert_eq!(error.kind(), EvidenceErrorKind::SecretDetected);
    assert!(!fixture.0.join("output").exists());
}

#[test]
fn offline_verification_detects_tampering_and_unlisted_files() {
    let fixture = TestDirectory::new("tamper");
    let artifact = fixture.0.join("runtime.log");
    fs::write(&artifact, b"worker completed\n").expect("write Artifact fixture");
    let report = export_evidence(
        &fixture.0.join("output"),
        &request(&artifact, "evidence-tamper"),
    )
    .expect("export fixture");

    fs::write(report.package_path.join("verdict.json"), b"{}\n").expect("tamper verdict");
    let error = verify_evidence_package(&report.package_path).expect_err("tamper must fail");
    assert_eq!(error.kind(), EvidenceErrorKind::Corrupt);

    fs::write(report.package_path.join("unlisted.txt"), b"extra\n").expect("add unlisted file");
    let error = verify_evidence_package(&report.package_path).expect_err("unlisted file must fail");
    assert_eq!(error.kind(), EvidenceErrorKind::Corrupt);
}
